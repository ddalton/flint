{{- define "flint-nfs-client.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end }}

{{- define "flint-nfs-client.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-nfs-client.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-nfs-client.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{ include "flint-nfs-client.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "flint-nfs-client.image" -}}
{{- if .Values.image.ref }}{{ .Values.image.ref }}
{{- else }}
{{- $reg := coalesce .Values.global.imageRegistry .Values.image.registry -}}
{{- $tag := .Values.image.tag | default .Chart.AppVersion -}}
{{- if $reg }}{{ printf "%s/%s:%s" $reg .Values.image.repository $tag }}{{ else }}{{ printf "%s:%s" .Values.image.repository $tag }}{{ end -}}
{{- end }}
{{- end }}
