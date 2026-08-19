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

{{/*
The server config, verbatim — the ConfigMap's `mds.yaml`.

It lives in a named template for one reason: the Deployment hashes it
into `checksum/config`, so a `helm upgrade` that changes ANY of these
values rolls the hub. Without that the ConfigMap updates and nothing
else happens: the server parses `--config` exactly once at boot and has
no reload path, so the new setting reaches the running hub NEVER (and
`kubectl get cm` shows the new value, which is the worst kind of
wrong). Same reason the operator carries the same annotation.
*/}}
{{- define "flint-lite.mdsYaml" -}}
apiVersion: flint.io/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind:
    address: "0.0.0.0"
    port: 2049
  # Inert in standalone (no layouts are ever granted) but part of the
  # config schema.
  layout:
    type: file
    stripeSize: 8388608
    policy: stripe
  dataServers: []
  state:
    backend: sqlite
    config:
      path: /data/state/state.db
  {{- if .Values.tier.enabled }}
  # S3 cold tier (a field of the mds: section — the parser IGNORES
  # unknown top-level keys, so misplacing this renders a silently
  # tierless hub; the kind tier e2e's leg 1 pins the placement).
  # The volume epoch is claimed BEFORE the listener binds (an
  # unfenced hub never serves); ".flint/" under the prefix is
  # reserved for tier control objects. Unset knobs take the server
  # defaults — the economics gate's assumptions.
  tier:
    enabled: true
    bucket: {{ .Values.tier.bucket | quote }}
    {{- with .Values.tier.keyPrefix }}
    keyPrefix: {{ . | quote }}
    {{- end }}
    {{- with .Values.tier.endpoint }}
    endpoint: {{ . | quote }}
    {{- end }}
    importOnStart: {{ .Values.tier.importOnStart }}
    {{- with .Values.tier.settings }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- end }}
exports:
  - path: /data/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: rw
logging:
  level: {{ .Values.logLevel | default "info" }}
  format: text
{{- end }}
