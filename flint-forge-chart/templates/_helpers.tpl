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

{{- define "flint-forge.serverTag" -}}
{{- default (default .Chart.AppVersion .Values.image.tag) .Values.server.tag -}}
{{- end -}}

{{- define "flint-forge.gitImage" -}}
{{- if .Values.server.gitImage -}}
{{- .Values.server.gitImage -}}
{{- else -}}
{{- printf "%s/flint-forge-git:%s" .Values.server.repository (include "flint-forge.serverTag" .) -}}
{{- end -}}
{{- end -}}

{{- define "flint-forge.syncerImage" -}}
{{- if .Values.server.syncerImage -}}
{{- .Values.server.syncerImage -}}
{{- else -}}
{{- printf "%s/flint-forge-syncer:%s" .Values.server.repository (include "flint-forge.serverTag" .) -}}
{{- end -}}
{{- end -}}

{{- define "flint-forge.doorName" -}}
{{- printf "%s-door" (include "flint-forge.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "flint-forge.doorLabels" -}}
app.kubernetes.io/name: {{ include "flint-forge.doorName" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "flint-forge.doorSelectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-forge.doorName" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
