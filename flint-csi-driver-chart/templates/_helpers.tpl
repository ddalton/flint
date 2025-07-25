{{/*
Expand the name of the chart.
*/}}
{{- define "flint-csi-driver-chart.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "flint-csi-driver-chart.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "flint-csi-driver-chart.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "flint-csi-driver-chart.labels" -}}
helm.sh/chart: {{ include "flint-csi-driver-chart.chart" . }}
{{ include "flint-csi-driver-chart.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "flint-csi-driver-chart.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-csi-driver-chart.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "flint-csi-driver-chart.serviceAccountName.controller" -}}
{{- if .Values.serviceAccount.controller.create }}
{{- default (printf "%s-controller" (include "flint-csi-driver-chart.fullname" .)) .Values.serviceAccount.controller.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.controller.name }}
{{- end }}
{{- end -}}

{{- define "flint-csi-driver-chart.serviceAccountName.node" -}}
{{- if .Values.serviceAccount.node.create }}
{{- default (printf "%s-node" (include "flint-csi-driver-chart.fullname" .)) .Values.serviceAccount.node.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.node.name }}
{{- end }}
{{- end -}}

{{/*
Custom Resource Namespace - determines where SpdkDisk, SpdkVolume, SpdkSnapshot should be created
*/}}
{{- define "flint-csi-driver-chart.customResourceNamespace" -}}
{{- if .Values.driver.customResourceNamespace }}
{{- .Values.driver.customResourceNamespace }}
{{- else }}
{{- .Release.Namespace }}
{{- end }}
{{- end -}}
