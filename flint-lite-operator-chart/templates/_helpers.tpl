{{/* Chart name. */}}
{{- define "flint-lite-operator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-lite-operator.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-lite-operator.labels" -}}
helm.sh/chart: {{ include "flint-lite-operator.chart" . }}
{{ include "flint-lite-operator.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "flint-lite-operator.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-lite-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-lite-operator.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "flint-lite-operator.name" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* The operator image reference. */}}
{{- define "flint-lite-operator.image" -}}
{{- if .Values.image.ref }}{{ .Values.image.ref }}
{{- else }}{{ printf "%s/%s:%s" .Values.image.repository .Values.image.name (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}
{{- end }}

{{/*
Default hub image. Empty hubImage tracks the chart's appVersion, so an
operator upgrade moves the fleet's hubs with it — which is the point of
having one default instead of N pinned specs.
*/}}
{{- define "flint-lite-operator.hubImage" -}}
{{- .Values.hubImage | default (printf "dilipdalton/flint-pnfs:%s" .Chart.AppVersion) }}
{{- end }}

{{/* flint-hub-gateway: its own name and labels, so it is never selected
     by the operator's Service or PDB and vice versa. */}}
{{- define "flint-lite-operator.gatewayName" -}}
{{- printf "%s-gateway" (include "flint-lite-operator.name" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-lite-operator.gatewaySelectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-lite-operator.gatewayName" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-lite-operator.gatewayLabels" -}}
helm.sh/chart: {{ include "flint-lite-operator.chart" . }}
{{ include "flint-lite-operator.gatewaySelectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}
