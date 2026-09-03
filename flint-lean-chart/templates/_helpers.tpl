{{- define "flint-lean.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-lean.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-lean.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-lean.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-lean.labels" -}}
helm.sh/chart: {{ include "flint-lean.chart" . }}
{{ include "flint-lean.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "flint-lean.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "flint-lean.name" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "flint-lean.image" -}}
{{- if .Values.image.ref }}{{ .Values.image.ref }}
{{- else }}{{ printf "%s/%s:%s" .Values.image.repository .Values.image.name (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}
{{- end }}

{{/* The gateway is its own workload: separate name and selector from
     the operator's. */}}
{{- define "flint-lean.gatewayName" -}}
{{- printf "%s-gateway" (include "flint-lean.name" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-lean.gatewaySelectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-lean.gatewayName" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
