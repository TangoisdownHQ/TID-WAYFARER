package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// OutpostSpec defines the desired state of an Outpost.
type OutpostSpec struct {
	// Role distinguishes the Core HQ from a regional/surface outpost.
	// +kubebuilder:validation:Enum=core;outpost
	// +kubebuilder:default=outpost
	Role string `json:"role,omitempty"`

	// BodyID is the NAIF body id the outpost sits on
	// (Earth=399, Moon=301, Mars=499, Jupiter=599 ...).
	// +kubebuilder:default=399
	BodyID int32 `json:"bodyId,omitempty"`

	// Region is a human label ("US-East", "UK-London", "Mars-Jezero").
	Region string `json:"region,omitempty"`

	// Replicas is the API replica count. Keep at 1 unless keys persistence
	// is disabled or backed by ReadWriteMany storage.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=1
	Replicas int32 `json:"replicas,omitempty"`

	// Image selects the tid-wayfarer API container image.
	Image OutpostImage `json:"image,omitempty"`

	// Peers configures how this outpost joins the mesh on startup.
	Peers OutpostPeers `json:"peers,omitempty"`

	// Postgres configures the embedded database for this outpost.
	Postgres OutpostPostgres `json:"postgres,omitempty"`

	// SecretName references an existing Secret holding JWT_SECRET and
	// POSTGRES_PASSWORD. When empty the controller generates one.
	SecretName string `json:"secretName,omitempty"`

	// Migrate runs the bundled SQL migrations as a Job on create/update.
	// +kubebuilder:default=true
	Migrate bool `json:"migrate,omitempty"`
}

// OutpostImage selects the API container image.
type OutpostImage struct {
	// +kubebuilder:default="tid-wayfarer"
	Repository string `json:"repository,omitempty"`
	// +kubebuilder:default="latest"
	Tag string `json:"tag,omitempty"`
	// +kubebuilder:default="IfNotPresent"
	PullPolicy corev1.PullPolicy `json:"pullPolicy,omitempty"`
}

// OutpostPeers configures mesh registration.
type OutpostPeers struct {
	// CoreApiURL is the Core HQ registration endpoint an outpost calls on boot,
	// e.g. https://core.wayfarer.example.com/api/nodes/register.
	CoreApiURL string `json:"coreApiUrl,omitempty"`

	// FabricAuth selects how peer outposts authenticate to this one:
	// "both" (per-node Ed25519 signature or the shared token; the default,
	// for rolling an existing fabric onto signed auth), "signed" (signatures
	// only — the shared token then only admits a new node to
	// /api/nodes/register), or "legacy" (shared token only).
	// +kubebuilder:validation:Enum=both;signed;legacy
	FabricAuth string `json:"fabricAuth,omitempty"`
}

// OutpostPostgres configures the embedded database.
type OutpostPostgres struct {
	// +kubebuilder:default=true
	Enabled bool `json:"enabled,omitempty"`
	// +kubebuilder:default="postgres:16-alpine"
	Image string `json:"image,omitempty"`
	// +kubebuilder:default="postgres"
	User string `json:"user,omitempty"`
	// +kubebuilder:default="tidasone"
	Database string `json:"database,omitempty"`
	// +kubebuilder:default="5Gi"
	StorageSize string `json:"storageSize,omitempty"`
	// StorageClass empty means the cluster default.
	StorageClass string `json:"storageClass,omitempty"`
}

// OutpostStatus captures the observed state of an Outpost.
type OutpostStatus struct {
	// Phase is a coarse lifecycle summary: Pending | Provisioning | Ready | Degraded.
	Phase string `json:"phase,omitempty"`

	// ReadyReplicas is the number of ready API pods.
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// ObservedGeneration is the .metadata.generation last reconciled.
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`

	// Conditions represent the latest observations of the outpost's state.
	// +optional
	// +patchMergeKey=type
	// +patchStrategy=merge
	Conditions []metav1.Condition `json:"conditions,omitempty" patchStrategy:"merge" patchMergeKey:"type"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=op;outpost,categories=wayfarer
// +kubebuilder:printcolumn:name="Role",type=string,JSONPath=`.spec.role`
// +kubebuilder:printcolumn:name="Body",type=integer,JSONPath=`.spec.bodyId`
// +kubebuilder:printcolumn:name="Region",type=string,JSONPath=`.spec.region`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// Outpost is the Schema for the outposts API. One Outpost == one tid-wayfarer
// deployment (API + optional embedded Postgres) that syncs with the mesh.
type Outpost struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   OutpostSpec   `json:"spec,omitempty"`
	Status OutpostStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// OutpostList contains a list of Outpost.
type OutpostList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Outpost `json:"items"`
}

func init() {
	SchemeBuilder.Register(&Outpost{}, &OutpostList{})
}
