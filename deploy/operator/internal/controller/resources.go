package controller

import (
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/util/intstr"

	wayfarerv1alpha1 "github.com/TangoisdownHQ/tid-wayfarer-operator/api/v1alpha1"
)

const (
	apiPort         = 3000
	keysMountPath   = "/app/keys"
	migrationsPath  = "/app/migrations"
	componentLabel  = "wayfarer.tid.net/component"
	instanceLabel   = "app.kubernetes.io/name"
	partOfLabel     = "app.kubernetes.io/part-of"
	managedByLabel  = "app.kubernetes.io/managed-by"
	roleLabel       = "wayfarer.tid.net/outpost-role"
	bodyLabel       = "wayfarer.tid.net/body-id"
	regionLabel     = "wayfarer.tid.net/region"
	defaultDatabase = "tidasone"
)

// names centralises the derived resource names for one Outpost.
type names struct{ base string }

func namesFor(o *wayfarerv1alpha1.Outpost) names { return names{base: o.Name} }

func (n names) api() string        { return n.base + "-api" }
func (n names) config() string     { return n.base + "-config" }
func (n names) secret() string     { return n.base + "-secret" }
func (n names) postgres() string   { return n.base + "-postgres" }
func (n names) keysPVC() string    { return n.base + "-keys" }
func (n names) migrateJob() string { return n.base + "-migrate" }

// baseLabels are applied to every child object of an Outpost.
func baseLabels(o *wayfarerv1alpha1.Outpost) map[string]string {
	l := map[string]string{
		instanceLabel:  o.Name,
		partOfLabel:    "tid-wayfarer",
		managedByLabel: "tid-wayfarer-operator",
		roleLabel:      orDefault(o.Spec.Role, "outpost"),
		bodyLabel:      fmt.Sprintf("%d", o.Spec.BodyID),
	}
	if o.Spec.Region != "" {
		l[regionLabel] = o.Spec.Region
	}
	return l
}

func componentLabels(o *wayfarerv1alpha1.Outpost, component string) map[string]string {
	l := baseLabels(o)
	l[componentLabel] = component
	return l
}

// selectorLabels are the immutable subset used for pod selection.
func selectorLabels(o *wayfarerv1alpha1.Outpost, component string) map[string]string {
	return map[string]string{
		instanceLabel:  o.Name,
		componentLabel: component,
	}
}

func orDefault(v, def string) string {
	if v == "" {
		return def
	}
	return v
}

func dbName(o *wayfarerv1alpha1.Outpost) string {
	return orDefault(o.Spec.Postgres.Database, defaultDatabase)
}

func dbUser(o *wayfarerv1alpha1.Outpost) string {
	return orDefault(o.Spec.Postgres.User, "postgres")
}

func dbHost(o *wayfarerv1alpha1.Outpost) string {
	return namesFor(o).postgres()
}

// databaseURL assembles a DATABASE_URL where the password is substituted from
// the POSTGRES_PASSWORD env var at container start ($(VAR) expansion).
func databaseURL(o *wayfarerv1alpha1.Outpost) string {
	return fmt.Sprintf("postgres://%s:$(POSTGRES_PASSWORD)@%s:5432/%s",
		dbUser(o), dbHost(o), dbName(o))
}

func imageRef(o *wayfarerv1alpha1.Outpost) string {
	repo := orDefault(o.Spec.Image.Repository, "tid-wayfarer")
	tag := orDefault(o.Spec.Image.Tag, "latest")
	return repo + ":" + tag
}

func pullPolicy(o *wayfarerv1alpha1.Outpost) corev1.PullPolicy {
	if o.Spec.Image.PullPolicy != "" {
		return o.Spec.Image.PullPolicy
	}
	return corev1.PullIfNotPresent
}

// buildConfigMap holds the non-secret environment for the outpost.
func buildConfigMap(o *wayfarerv1alpha1.Outpost) *corev1.ConfigMap {
	n := namesFor(o)
	return &corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.config(),
			Namespace: o.Namespace,
			Labels:    componentLabels(o, "api"),
		},
		Data: map[string]string{
			"OUTPOST_NAME":   o.Name,
			"HOST":           "0.0.0.0",
			"PORT":           fmt.Sprintf("%d", apiPort),
			"BODY_ID":        fmt.Sprintf("%d", o.Spec.BodyID),
			"OUTPOST_ROLE":   orDefault(o.Spec.Role, "outpost"),
			"OUTPOST_REGION": o.Spec.Region,
			"CORE_API_URL":   o.Spec.Peers.CoreApiURL,
			"FABRIC_AUTH":    orDefault(o.Spec.Peers.FabricAuth, "both"),
		},
	}
}

// buildPostgresService is the headless service backing the StatefulSet.
func buildPostgresService(o *wayfarerv1alpha1.Outpost) *corev1.Service {
	n := namesFor(o)
	return &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.postgres(),
			Namespace: o.Namespace,
			Labels:    componentLabels(o, "postgres"),
		},
		Spec: corev1.ServiceSpec{
			ClusterIP: corev1.ClusterIPNone,
			Selector:  selectorLabels(o, "postgres"),
			Ports:     []corev1.ServicePort{{Name: "postgres", Port: 5432, TargetPort: intstr.FromInt32(5432)}},
		},
	}
}

// buildPostgresStatefulSet is the embedded database.
func buildPostgresStatefulSet(o *wayfarerv1alpha1.Outpost) *appsv1.StatefulSet {
	n := namesFor(o)
	labels := componentLabels(o, "postgres")
	storage := orDefault(o.Spec.Postgres.StorageSize, "5Gi")
	image := orDefault(o.Spec.Postgres.Image, "postgres:16-alpine")
	replicas := int32(1)

	pvc := corev1.PersistentVolumeClaim{
		ObjectMeta: metav1.ObjectMeta{Name: "data"},
		Spec: corev1.PersistentVolumeClaimSpec{
			AccessModes: []corev1.PersistentVolumeAccessMode{corev1.ReadWriteOnce},
			Resources: corev1.VolumeResourceRequirements{
				Requests: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse(storage)},
			},
		},
	}
	if o.Spec.Postgres.StorageClass != "" {
		sc := o.Spec.Postgres.StorageClass
		pvc.Spec.StorageClassName = &sc
	}

	return &appsv1.StatefulSet{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.postgres(),
			Namespace: o.Namespace,
			Labels:    labels,
		},
		Spec: appsv1.StatefulSetSpec{
			ServiceName: n.postgres(),
			Replicas:    &replicas,
			Selector:    &metav1.LabelSelector{MatchLabels: selectorLabels(o, "postgres")},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labels},
				Spec: corev1.PodSpec{
					Containers: []corev1.Container{{
						Name:  "postgres",
						Image: image,
						Ports: []corev1.ContainerPort{{Name: "postgres", ContainerPort: 5432}},
						Env: []corev1.EnvVar{
							{Name: "POSTGRES_USER", Value: dbUser(o)},
							{Name: "POSTGRES_DB", Value: dbName(o)},
							{Name: "POSTGRES_PASSWORD", ValueFrom: secretRef(n.secret(), "POSTGRES_PASSWORD")},
							{Name: "PGDATA", Value: "/var/lib/postgresql/data/pgdata"},
						},
						VolumeMounts: []corev1.VolumeMount{{Name: "data", MountPath: "/var/lib/postgresql/data"}},
						ReadinessProbe: &corev1.Probe{
							ProbeHandler: corev1.ProbeHandler{Exec: &corev1.ExecAction{
								Command: []string{"pg_isready", "-U", dbUser(o), "-d", dbName(o)},
							}},
							InitialDelaySeconds: 5,
							PeriodSeconds:       5,
						},
					}},
				},
			},
			VolumeClaimTemplates: []corev1.PersistentVolumeClaim{pvc},
		},
	}
}

// buildKeysPVC persists node identity keys so node_id survives pod restarts.
func buildKeysPVC(o *wayfarerv1alpha1.Outpost) *corev1.PersistentVolumeClaim {
	n := namesFor(o)
	pvc := &corev1.PersistentVolumeClaim{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.keysPVC(),
			Namespace: o.Namespace,
			Labels:    componentLabels(o, "keys"),
		},
		Spec: corev1.PersistentVolumeClaimSpec{
			AccessModes: []corev1.PersistentVolumeAccessMode{corev1.ReadWriteOnce},
			Resources: corev1.VolumeResourceRequirements{
				Requests: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("1Gi")},
			},
		},
	}
	if o.Spec.Postgres.StorageClass != "" {
		sc := o.Spec.Postgres.StorageClass
		pvc.Spec.StorageClassName = &sc
	}
	return pvc
}

// buildAPIDeployment is the tid-wayfarer API workload.
func buildAPIDeployment(o *wayfarerv1alpha1.Outpost) *appsv1.Deployment {
	n := namesFor(o)
	labels := componentLabels(o, "api")
	replicas := o.Spec.Replicas
	if replicas < 1 {
		replicas = 1
	}

	container := corev1.Container{
		Name:            "api",
		Image:           imageRef(o),
		ImagePullPolicy: pullPolicy(o),
		Ports:           []corev1.ContainerPort{{Name: "http", ContainerPort: apiPort}},
		EnvFrom:         []corev1.EnvFromSource{{ConfigMapRef: &corev1.ConfigMapEnvSource{LocalObjectReference: corev1.LocalObjectReference{Name: n.config()}}}},
		Env: []corev1.EnvVar{
			{Name: "POSTGRES_PASSWORD", ValueFrom: secretRef(n.secret(), "POSTGRES_PASSWORD")},
			{Name: "JWT_SECRET", ValueFrom: secretRef(n.secret(), "JWT_SECRET")},
			// Optional so pods still start against Secrets created before this
			// key existed; the API fails closed on node-token auth without it.
			{Name: "NODE_SHARED_SECRET", ValueFrom: optionalSecretRef(n.secret(), "NODE_SHARED_SECRET")},
			{Name: "DATABASE_URL", Value: databaseURL(o)},
		},
		VolumeMounts:   []corev1.VolumeMount{{Name: "keys", MountPath: keysMountPath}},
		StartupProbe:   httpProbe("/api/health", 30, 5, 0),
		LivenessProbe:  httpProbe("/healthz", 3, 10, 30),
		ReadinessProbe: httpProbe("/api/health", 3, 5, 5),
	}

	return &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.api(),
			Namespace: o.Namespace,
			Labels:    labels,
		},
		Spec: appsv1.DeploymentSpec{
			Replicas: &replicas,
			Selector: &metav1.LabelSelector{MatchLabels: selectorLabels(o, "api")},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labels},
				Spec: corev1.PodSpec{
					Containers: []corev1.Container{container},
					Volumes: []corev1.Volume{{
						Name: "keys",
						VolumeSource: corev1.VolumeSource{
							PersistentVolumeClaim: &corev1.PersistentVolumeClaimVolumeSource{ClaimName: n.keysPVC()},
						},
					}},
				},
			},
		},
	}
}

// buildAPIService exposes the API inside the cluster.
func buildAPIService(o *wayfarerv1alpha1.Outpost) *corev1.Service {
	n := namesFor(o)
	return &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.api(),
			Namespace: o.Namespace,
			Labels:    componentLabels(o, "api"),
		},
		Spec: corev1.ServiceSpec{
			Type:     corev1.ServiceTypeClusterIP,
			Selector: selectorLabels(o, "api"),
			Ports:    []corev1.ServicePort{{Name: "http", Port: apiPort, TargetPort: intstr.FromString("http")}},
		},
	}
}

// buildMigrateJob applies the bundled SQL migrations using the API image
// (psql + /app/migrations are baked into the runtime stage).
func buildMigrateJob(o *wayfarerv1alpha1.Outpost) *batchv1.Job {
	n := namesFor(o)
	backoff := int32(6)
	ttl := int32(600)
	script := fmt.Sprintf(
		`set -e; for f in $(ls %s/*.sql | sort); do echo "applying $f"; psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f"; done; echo "migrations complete"`,
		migrationsPath,
	)
	return &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{
			Name:      n.migrateJob(),
			Namespace: o.Namespace,
			Labels:    componentLabels(o, "migrate"),
		},
		Spec: batchv1.JobSpec{
			BackoffLimit:            &backoff,
			TTLSecondsAfterFinished: &ttl,
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: componentLabels(o, "migrate")},
				Spec: corev1.PodSpec{
					RestartPolicy: corev1.RestartPolicyNever,
					Containers: []corev1.Container{{
						Name:            "migrate",
						Image:           imageRef(o),
						ImagePullPolicy: pullPolicy(o),
						Command:         []string{"/bin/sh", "-c", script},
						Env: []corev1.EnvVar{
							{Name: "POSTGRES_PASSWORD", ValueFrom: secretRef(n.secret(), "POSTGRES_PASSWORD")},
							{Name: "DATABASE_URL", Value: databaseURL(o)},
						},
					}},
				},
			},
		},
	}
}

func secretRef(name, key string) *corev1.EnvVarSource {
	return &corev1.EnvVarSource{
		SecretKeyRef: &corev1.SecretKeySelector{
			LocalObjectReference: corev1.LocalObjectReference{Name: name},
			Key:                  key,
		},
	}
}

func optionalSecretRef(name, key string) *corev1.EnvVarSource {
	optional := true
	ref := secretRef(name, key)
	ref.SecretKeyRef.Optional = &optional
	return ref
}

func httpProbe(path string, failureThreshold, period, initialDelay int32) *corev1.Probe {
	return &corev1.Probe{
		ProbeHandler: corev1.ProbeHandler{
			HTTPGet: &corev1.HTTPGetAction{Path: path, Port: intstr.FromString("http")},
		},
		FailureThreshold:    failureThreshold,
		PeriodSeconds:       period,
		InitialDelaySeconds: initialDelay,
	}
}
