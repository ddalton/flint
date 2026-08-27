{{- define "flint-passthrough.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-passthrough.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-passthrough.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-passthrough.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-passthrough.labels" -}}
helm.sh/chart: {{ include "flint-passthrough.chart" . }}
{{ include "flint-passthrough.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "flint-passthrough.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "flint-passthrough.name" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "flint-passthrough.image" -}}
{{- if .Values.image.ref }}{{ .Values.image.ref }}
{{- else }}{{ printf "%s/%s:%s" .Values.image.repository .Values.image.name (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}
{{- end }}

{{- define "flint-passthrough.sidecarImage" -}}
{{- if .Values.sidecarImage.ref }}{{ .Values.sidecarImage.ref }}
{{- else }}{{ printf "%s/%s:%s" .Values.sidecarImage.repository .Values.sidecarImage.name (.Values.sidecarImage.tag | default .Chart.AppVersion) }}
{{- end }}
{{- end }}
