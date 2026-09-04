{{- define "flint-forge.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flint-forge.labels" -}}
app.kubernetes.io/name: {{ include "flint-forge.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "flint-forge.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-forge.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "flint-forge.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "flint-forge.name" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "flint-forge.image" -}}
{{- if .Values.image.ref -}}
{{- .Values.image.ref -}}
{{- else -}}
{{- printf "%s/%s:%s" .Values.image.repository .Values.image.name (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}
{{- end -}}
