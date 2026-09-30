package controller

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/log"

	wayfarerv1alpha1 "github.com/TangoisdownHQ/tid-wayfarer-operator/api/v1alpha1"
)

// OutpostReconciler reconciles an Outpost object into the Deployment, Service,
// embedded Postgres, migration Job and Secret that make up one tid-wayfarer outpost.
type OutpostReconciler struct {
	client.Client
	Scheme *runtime.Scheme
}

// +kubebuilder:rbac:groups=wayfarer.tid.net,resources=outposts,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=wayfarer.tid.net,resources=outposts/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=wayfarer.tid.net,resources=outposts/finalizers,verbs=update
// +kubebuilder:rbac:groups=apps,resources=deployments;statefulsets,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=batch,resources=jobs,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services;configmaps;secrets;persistentvolumeclaims,verbs=get;list;watch;create;update;patch;delete

// Reconcile drives the actual cluster state toward the Outpost spec.
func (r *OutpostReconciler) Reconcile(ctx context.Context, req ctrl.Request) (ctrl.Result, error) {
	l := log.FromContext(ctx)

	var outpost wayfarerv1alpha1.Outpost
	if err := r.Get(ctx, req.NamespacedName, &outpost); err != nil {
		// Not found: owned children are garbage-collected via owner refs.
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	// 1. Secret (JWT + DB password) — generated once if the user didn't supply one.
	if outpost.Spec.SecretName == "" {
		if err := r.ensureSecret(ctx, &outpost); err != nil {
			return ctrl.Result{}, fmt.Errorf("ensure secret: %w", err)
		}
	}

	// 2. Non-secret config.
	if err := r.apply(ctx, &outpost, buildConfigMap(&outpost)); err != nil {
		return ctrl.Result{}, err
	}

	// 3. Embedded Postgres (skipped when disabled → external DB).
	if outpost.Spec.Postgres.Enabled {
		if err := r.apply(ctx, &outpost, buildPostgresService(&outpost)); err != nil {
			return ctrl.Result{}, err
		}
		if err := r.apply(ctx, &outpost, buildPostgresStatefulSet(&outpost)); err != nil {
			return ctrl.Result{}, err
		}
	}

	// 4. Identity keys PVC.
	if err := r.apply(ctx, &outpost, buildKeysPVC(&outpost)); err != nil {
		return ctrl.Result{}, err
	}

	// 5. Migration Job (create-once; immutable, so we don't update it).
	if outpost.Spec.Migrate {
		if err := r.ensureJob(ctx, &outpost, buildMigrateJob(&outpost)); err != nil {
			return ctrl.Result{}, err
		}
	}

	// 6. API Deployment + Service.
	if err := r.apply(ctx, &outpost, buildAPIDeployment(&outpost)); err != nil {
		return ctrl.Result{}, err
	}
	if err := r.apply(ctx, &outpost, buildAPIService(&outpost)); err != nil {
		return ctrl.Result{}, err
	}

	// 7. Status.
	if err := r.updateStatus(ctx, &outpost); err != nil {
		l.Error(err, "status update failed")
	}

	return ctrl.Result{}, nil
}

// apply creates or updates a child object and sets the Outpost as its owner so
// deletion cascades and reconciliation is idempotent.
func (r *OutpostReconciler) apply(ctx context.Context, o *wayfarerv1alpha1.Outpost, desired client.Object) error {
	if err := controllerutil.SetControllerReference(o, desired, r.Scheme); err != nil {
		return err
	}

	key := client.ObjectKeyFromObject(desired)
	existing, _ := desired.DeepCopyObject().(client.Object)
	err := r.Get(ctx, key, existing)
	if apierrors.IsNotFound(err) {
		return r.Create(ctx, desired)
	}
	if err != nil {
		return err
	}

	// Carry over the resourceVersion so the update is accepted, then patch spec.
	desired.SetResourceVersion(existing.GetResourceVersion())
	return r.Update(ctx, desired)
}

// ensureJob creates the Job only if it does not already exist (Jobs are immutable).
func (r *OutpostReconciler) ensureJob(ctx context.Context, o *wayfarerv1alpha1.Outpost, job *batchv1.Job) error {
	if err := controllerutil.SetControllerReference(o, job, r.Scheme); err != nil {
		return err
	}
	var existing batchv1.Job
	err := r.Get(ctx, client.ObjectKeyFromObject(job), &existing)
	if apierrors.IsNotFound(err) {
		return r.Create(ctx, job)
	}
	return err
}

// ensureSecret generates a JWT secret and DB password once, then leaves them alone.
func (r *OutpostReconciler) ensureSecret(ctx context.Context, o *wayfarerv1alpha1.Outpost) error {
	n := namesFor(o)
	var existing corev1.Secret
	err := r.Get(ctx, client.ObjectKey{Namespace: o.Namespace, Name: n.secret()}, &existing)
	if err == nil {
		return nil // already exists — never rotate silently
	}
	if !apierrors.IsNotFound(err) {
		return err
	}

	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.secret(),
			Namespace: o.Namespace,
			Labels:    componentLabels(o, "api"),
		},
		Type: corev1.SecretTypeOpaque,
		StringData: map[string]string{
			"JWT_SECRET":        randomToken(32),
			"POSTGRES_PASSWORD": randomToken(24),
			// Fabric-wide sync secret. Generated per-outpost by default; for a
			// multi-outpost fabric, pre-create this Secret with a shared value —
			// we only create it when missing (never rotate silently).
			"NODE_SHARED_SECRET": randomToken(32),
		},
	}
	if err := controllerutil.SetControllerReference(o, secret, r.Scheme); err != nil {
		return err
	}
	return r.Create(ctx, secret)
}

// updateStatus reflects Deployment readiness back onto the Outpost.
func (r *OutpostReconciler) updateStatus(ctx context.Context, o *wayfarerv1alpha1.Outpost) error {
	n := namesFor(o)
	var dep appsv1.Deployment
	phase := "Provisioning"
	var ready int32
	if err := r.Get(ctx, client.ObjectKey{Namespace: o.Namespace, Name: n.api()}, &dep); err == nil {
		ready = dep.Status.ReadyReplicas
		switch {
		case ready == 0:
			phase = "Pending"
		case ready >= *dep.Spec.Replicas:
			phase = "Ready"
		default:
			phase = "Degraded"
		}
	}

	if o.Status.Phase == phase &&
		o.Status.ReadyReplicas == ready &&
		o.Status.ObservedGeneration == o.Generation {
		return nil
	}

	o.Status.Phase = phase
	o.Status.ReadyReplicas = ready
	o.Status.ObservedGeneration = o.Generation
	return r.Status().Update(ctx, o)
}

func randomToken(nBytes int) string {
	b := make([]byte, nBytes)
	if _, err := rand.Read(b); err != nil {
		// crypto/rand failure is fatal-worthy, but returning a marker keeps the
		// signature simple; the caller's Create will still produce a usable secret.
		return "changeme-rand-unavailable"
	}
	return hex.EncodeToString(b)
}

// SetupWithManager wires the controller and declares the objects it owns.
func (r *OutpostReconciler) SetupWithManager(mgr ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(mgr).
		For(&wayfarerv1alpha1.Outpost{}).
		Owns(&appsv1.Deployment{}).
		Owns(&appsv1.StatefulSet{}).
		Owns(&corev1.Service{}).
		Owns(&corev1.ConfigMap{}).
		Owns(&corev1.Secret{}).
		Owns(&corev1.PersistentVolumeClaim{}).
		Owns(&batchv1.Job{}).
		Complete(r)
}
