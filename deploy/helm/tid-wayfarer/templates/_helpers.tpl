{{/* ────────────────────────────────────────────────
     tid-wayfarer chart helpers
────────────────────────────────────────────────────*/}}

{{/* Fully qualified app name (release-name+chart, capped at 63 chars) */}}
{{- define "tid-wayfarer.fullname" -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end -}}

{{/* Common labels — applied to every resource */}}
{{- define "tid-wayfarer.labels" -}}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
app.kubernetes.io/name: tid-wayfarer
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
wayfarer.tid.net/outpost-role: {{ .Values.outpost.role | quote }}
wayfarer.tid.net/body-id: {{ .Values.outpost.bodyId | quote }}
{{- if .Values.outpost.location.region }}
wayfarer.tid.net/region: {{ .Values.outpost.location.region | quote }}
{{- end }}
{{- end -}}

{{/* Selector labels — minimal, for matchLabels */}}
{{- define "tid-wayfarer.selectorLabels" -}}
app.kubernetes.io/name: tid-wayfarer
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Postgres host — embedded service name or external override */}}
{{- define "tid-wayfarer.dbHost" -}}
{{- if .Values.postgres.enabled -}}
{{ .Release.Name }}-postgres
{{- else -}}
{{ .Values.externalDatabase.host }}
{{- end -}}
{{- end -}}

{{/* DATABASE_URL — assembled at runtime so $POSTGRES_PASSWORD comes from env */}}
{{- define "tid-wayfarer.databaseUrl" -}}
{{- if .Values.postgres.enabled -}}
postgres://{{ .Values.postgres.user }}:$(POSTGRES_PASSWORD)@{{ include "tid-wayfarer.dbHost" . }}:5432/{{ .Values.postgres.database }}
{{- else -}}
postgres://{{ .Values.externalDatabase.user }}:$(POSTGRES_PASSWORD)@{{ .Values.externalDatabase.host }}:{{ .Values.externalDatabase.port }}/{{ .Values.externalDatabase.database }}
{{- end -}}
{{- end -}}
