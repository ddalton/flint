{{- define "flint-s3-csi.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "flint-s3-csi.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Common labels WITHOUT a name: the node DaemonSet and the broker are
     two workloads with two selectors, and a shared app.kubernetes.io/name
     would let the broker's Service pick up node pods. */}}
{{- define "flint-s3-csi.labels" -}}
helm.sh/chart: {{ include "flint-s3-csi.chart" . }}
app.kubernetes.io/part-of: {{ include "flint-s3-csi.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/* FIXED NAMES, deliberately not derived from the release name. The
     driver name is compiled into the binary (s3csi::DRIVER_NAME) and is
     the registrar path, the plugin dir AND the token audience; the node
     ServiceAccount is named by the broker's FLINT_S3B_NODE_PRINCIPAL and
     by the admission policy; the broker Service is named by the node
     plugin's FLINT_S3CSI_BROKER_URL. One release per cluster. */}}
{{- define "flint-s3-csi.driverName" -}}s3.csi.chert.us{{- end }}
{{- define "flint-s3-csi.node.name" -}}flint-s3-csi-node{{- end }}
{{- define "flint-s3-csi.broker.name" -}}flint-s3-broker{{- end }}

{{/* The kubelet root and the plugin's directory under it. Five places
     must agree — the Bidirectional hostPath, the registrar's
     --kubelet-registration-path, the plugin socket dir, the node token
     env, and the admission policy's hostPath prefix — so they are
     spelled once here. The binary defaults to the same paths
     (s3csi::kubelet_root / plugin_root). */}}
{{- define "flint-s3-csi.kubeletDir" -}}/var/lib/kubelet{{- end }}
{{- define "flint-s3-csi.pluginDir" -}}
{{- include "flint-s3-csi.kubeletDir" . }}/plugins/{{ include "flint-s3-csi.driverName" . }}
{{- end }}

{{- define "flint-s3-csi.node.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-s3-csi.node.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-s3-csi.node.labels" -}}
{{ include "flint-s3-csi.labels" . }}
app.kubernetes.io/name: {{ include "flint-s3-csi.node.name" . }}
app.kubernetes.io/component: node
{{- end }}

{{- define "flint-s3-csi.broker.selectorLabels" -}}
app.kubernetes.io/name: {{ include "flint-s3-csi.broker.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "flint-s3-csi.broker.labels" -}}
{{ include "flint-s3-csi.labels" . }}
app.kubernetes.io/name: {{ include "flint-s3-csi.broker.name" . }}
app.kubernetes.io/component: broker
{{- end }}

{{/* Images: {repository}:{tag|appVersion}. The node plugin and the
     broker run from ONE image (different binaries). */}}
{{- define "flint-s3-csi.node.image" -}}
{{- printf "%s:%s" .Values.node.image.repository (.Values.node.image.tag | default .Chart.AppVersion) }}
{{- end }}

{{- define "flint-s3-csi.passthroughImage" -}}
{{- printf "%s:%s" .Values.workers.passthroughImage.repository (.Values.workers.passthroughImage.tag | default .Chart.AppVersion) }}
{{- end }}

{{- define "flint-s3-csi.leanImage" -}}
{{- printf "%s:%s" .Values.workers.leanImage.repository (.Values.workers.leanImage.tag | default .Chart.AppVersion) }}
{{- end }}

{{/* The node plugin's principal as TokenReview and the audit log spell
     it: what the broker admits to register publishes, and what the
     admission policy admits to create workers. */}}
{{- define "flint-s3-csi.nodePrincipal" -}}
system:serviceaccount:{{ .Release.Namespace }}:{{ include "flint-s3-csi.node.name" . }}
{{- end }}

{{/* Plain http on the pod network: the bearer the plugin presents is a
     1-hour, audience-bound ServiceAccount token, and what comes back is
     a 15-minute credential. Terminate TLS at the broker
     (FLINT_S3B_TLS_CERT/_KEY) and point the plugin's
     FLINT_S3CSI_BROKER_CA at the CA if the pod network is not trusted;
     the chart does not wire that yet. */}}
{{- define "flint-s3-csi.brokerUrl" -}}
http://{{ include "flint-s3-csi.broker.name" . }}.{{ .Release.Namespace }}.svc:80
{{- end }}

{{/* A YAML list of strings as a CEL list literal: ['a', 'b']. */}}
{{- define "flint-s3-csi.celStringList" -}}
{{- $out := list -}}
{{- range . -}}{{- $out = append $out (printf "'%s'" .) -}}{{- end -}}
{{- join ", " $out -}}
{{- end -}}
