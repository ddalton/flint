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

{{/* Images: {registry}/{repository}:{tag|appVersion} — empty registry => the
     repository's implicit registry (Docker Hub); global.imageRegistry overrides
     each image's own registry. The node plugin and the broker run from ONE
     image (different binaries). */}}
{{- define "flint-s3-csi.imageRef" -}}
{{- $reg := coalesce .root.Values.global.imageRegistry .img.registry -}}
{{- $tag := .img.tag | default .root.Chart.AppVersion -}}
{{- if $reg }}{{ printf "%s/%s:%s" $reg .img.repository $tag }}{{ else }}{{ printf "%s:%s" .img.repository $tag }}{{ end -}}
{{- end }}

{{- define "flint-s3-csi.node.image" -}}
{{- include "flint-s3-csi.imageRef" (dict "root" . "img" .Values.node.image) -}}
{{- end }}

{{- define "flint-s3-csi.passthroughImage" -}}
{{- include "flint-s3-csi.imageRef" (dict "root" . "img" .Values.workers.passthroughImage) -}}
{{- end }}

{{- define "flint-s3-csi.leanImage" -}}
{{- include "flint-s3-csi.imageRef" (dict "root" . "img" .Values.workers.leanImage) -}}
{{- end }}

{{/* A name a Secret or a ServiceAccount can have: a DNS-1123 subdomain. */}}
{{- define "flint-s3-csi.dnsSubdomainRe" -}}
^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$
{{- end }}

{{/* One component's image pull secrets (issue #85), as Secret names
     joined with ",", or "" for none. Call with (dict "root" $ "own"
     <the component's list> "key" "<its values key>"). The component's own
     list wins whenever it is set, and an empty list means none, whatever
     global says. A null list (the default) means global.imagePullSecrets.
     An entry is a name, or a map with a `name` key. A name no Secret can
     have is refused here, where the message can name the value. Otherwise
     kubelet would find no such Secret and pull with no credential. */}}
{{- define "flint-s3-csi.pullSecrets" -}}
{{- $key := .key -}}
{{- $list := .own -}}
{{- if kindIs "invalid" $list -}}
{{- $key = "global.imagePullSecrets" -}}
{{- $list = .root.Values.global.imagePullSecrets -}}
{{- end -}}
{{- if and $list (not (kindIs "slice" $list)) -}}
{{- fail (printf "%s must be a list of Secret names, e.g. [regcred]; got %q" $key (toString $list)) -}}
{{- end -}}
{{- $re := include "flint-s3-csi.dnsSubdomainRe" .root -}}
{{- $names := list -}}
{{- range $list -}}
{{- $n := . -}}
{{- if kindIs "map" . }}{{ $n = .name | default "" }}{{ end -}}
{{- $n = toString $n -}}
{{- if or (gt (len $n) 253) (not (regexMatch $re $n)) -}}
{{- fail (printf "%s: %q is not a Secret name (lowercase letters, digits, '-' and '.')" $key $n) -}}
{{- end -}}
{{- $names = append $names $n -}}
{{- end -}}
{{- join "," (uniq $names) -}}
{{- end }}

{{/* A pod spec's `imagePullSecrets:` block, unindented, or "" when the
     list is empty. Takes the same dict as flint-s3-csi.pullSecrets. */}}
{{- define "flint-s3-csi.podPullSecrets" -}}
{{- with include "flint-s3-csi.pullSecrets" . -}}
imagePullSecrets:
{{- range splitList "," . }}
  - name: {{ . | quote }}
{{- end }}
{{- end }}
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

{{/* A Kubernetes memory quantity in whole MiB (Gi, Mi, Ki, G, M, k, or
     plain bytes), or -1 for a suffix this helper does not read. Used to
     refuse a worker memory limit below mount-s3's own 512 MiB
     memory-target floor at render time, where the message can name the
     value, instead of as an OOM-killed mounter under the first read. */}}
{{- define "flint-s3-csi.quantityMiB" -}}
{{- $q := toString . -}}
{{- $num := regexFind "^[0-9.]+" $q | float64 -}}
{{- $suf := regexReplaceAll "^[0-9.]+" $q "" -}}
{{- $mib := -1.0 -}}
{{- if eq $suf "Gi" }}{{ $mib = mulf $num 1024 }}
{{- else if eq $suf "Mi" }}{{ $mib = $num }}
{{- else if eq $suf "Ki" }}{{ $mib = divf $num 1024 }}
{{- else if eq $suf "G" }}{{ $mib = divf (mulf $num 1000000000) 1048576 }}
{{- else if eq $suf "M" }}{{ $mib = divf (mulf $num 1000000) 1048576 }}
{{- else if eq $suf "k" }}{{ $mib = divf (mulf $num 1000) 1048576 }}
{{- else if eq $suf "" }}{{ $mib = divf $num 1048576 }}
{{- end -}}
{{- printf "%d" (int $mib) -}}
{{- end -}}
