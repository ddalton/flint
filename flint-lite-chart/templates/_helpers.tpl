{{/* Chart name. */}}
{{- define "flint-lite.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Chart name and version for the helm.sh/chart label. */}}
{{- define "flint-lite.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Common labels. */}}
{{- define "flint-lite.labels" -}}
helm.sh/chart: {{ include "flint-lite.chart" . }}
{{ include "flint-lite.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/* Selector labels. */}}
{{- define "flint-lite.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-lite.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
