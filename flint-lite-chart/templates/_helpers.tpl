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
apiVersion: chert.us/v1alpha1
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
    {{- with .Values.tier.ownerIdentity }}
    ownerIdentity: {{ . | quote }}
    {{- end }}
    {{- if .Values.tier.adoptData }}
    adoptData: true
    {{- end }}
    {{- with .Values.tier.settings }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- end }}
{{- /* `ro` or `rw` from .Values.readOnly; the server reads BOTH `options`
     and `access[].permissions` (the most specific network wins; a peer
     outside every network is refused; `/0` is anyone, either IP family). */}}
{{- $mode := ternary "ro" "rw" (default false .Values.readOnly) }}
exports:
  - path: /data/exports
    fsid: 1
    options: [{{ $mode }}, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: {{ $mode }}
logging:
  level: {{ .Values.logLevel | default "info" }}
  format: text
{{- if .Values.monitoring.enabled }}
# The hub's own HTTP surface, ClusterIP-only and NEVER on the
# consumer-facing Service — that Service carries NFS and can be made a
# LoadBalancer, which would publish the file API to the internet.
#
# /health and /status bind BEFORE the tier and before the NFS listener:
# the epoch claim can wait out a dead holder's lease and a DR import
# walks the whole bucket, and that whole window is invisible to anything
# watching only port 2049.
monitoring:
  health:
    enabled: true
    port: {{ .Values.monitoring.port }}
    path: {{ .Values.monitoring.healthPath | quote }}
  {{- if .Values.monitoring.fileApi.enabled }}
  # Browse and edit without mounting. Shares the health listener, and
  # refuses every request with 503 until the hub reaches phase Serving —
  # a listing taken mid-import would show a partial tree as though it
  # were the whole one.
  fileApi:
    enabled: true
    {{- if .Values.monitoring.fileApi.tokenSecret }}
    tokenFile: "/etc/flint/api-token/token"
    {{- end }}
    maxUploadBytes: {{ int64 .Values.monitoring.fileApi.maxUploadBytes }}
    maxDownloadBytes: {{ int64 .Values.monitoring.fileApi.maxDownloadBytes }}
    hydrateWaitSecs: {{ .Values.monitoring.fileApi.hydrateWaitSecs }}
    streamThresholdBytes: {{ int64 .Values.monitoring.fileApi.streamThresholdBytes }}
  {{- end }}
{{- end }}
{{- end }}

{{/*
Kerberos (security.kerberos): the same env, volumes and mounts, in the same
order, as the operator renders for a FlintShare's spec.security.kerberos
(lite_operator::kerberos::hub_pod_parts over nfs_krb5::server_pod_parts);
the parity test compares them. The keytab and the CA are mounted outside
/etc/flint, which is the config ConfigMap.
*/}}
{{- define "flint-lite.krb5Check" -}}
{{- $k := .Values.security.kerberos -}}
{{- $realm := required "security.kerberos.realm" $k.realm -}}
{{- if ne $realm (upper $realm) -}}{{ fail (printf "security.kerberos.realm %q: an upper-case realm name, e.g. CORP.EXAMPLE" $realm) }}{{- end -}}
{{- $_ := required "security.kerberos.realmRef" $k.realmRef -}}
{{- if not (has $k.minSec (list "krb5" "krb5i")) -}}{{ fail "security.kerberos.minSec: krb5 or krb5i (krb5p cannot be a floor: Linux does its state operations over krb5i)" }}{{- end -}}
{{- if not (has $k.deletionPolicy (list "Delete" "Retain")) -}}{{ fail "security.kerberos.deletionPolicy: Delete or Retain" }}{{- end -}}
{{- if $k.ldap.enabled -}}
{{- if not (or (hasPrefix "ldaps://" $k.ldap.url) (hasPrefix "ldap://" $k.ldap.url)) -}}{{ fail "security.kerberos.ldap.url: ldaps://host or ldap://host (StartTLS)" }}{{- end -}}
{{- if ne (empty $k.ldap.bindDN) (empty $k.ldap.bindPasswordSecret.name) -}}{{ fail "security.kerberos.ldap: bindDN and bindPasswordSecret.name go together" }}{{- end -}}
{{- if and $k.ldap.ca.configMap $k.ldap.ca.secret -}}{{ fail "security.kerberos.ldap.ca: a configMap or a secret, not both" }}{{- end -}}
{{- $_ := required "security.kerberos.ldap.userBase" $k.ldap.userBase -}}
{{- end -}}
{{- end }}

{{- define "flint-lite.krb5ServerNames" -}}
{{- $k := .Values.security.kerberos -}}
{{- $names := list (printf "flint-lite.%s.svc.%s" .Release.Namespace (trimAll "." $k.clusterDomain)) -}}
{{- range $k.serverNames -}}
{{- $n := lower (trimSuffix "." (trim .)) -}}
{{- if and $n (not (has $n $names)) }}{{ $names = append $names $n }}{{ end -}}
{{- end -}}
{{- toJson $names -}}
{{- end }}

{{- define "flint-lite.krb5Env" -}}
{{- $k := .Values.security.kerberos -}}
{{- $l := $k.ldap -}}
- name: KRB5_KTNAME
  value: /etc/flint-krb5/krb5.keytab
- name: FLINT_NFS_GSS_PRINCIPALS
{{- if $k.allowedPrincipals }}
  value: {{ join "," $k.allowedPrincipals | quote }}
{{- else }}
{{- $list := list (printf "host/*@%s" $k.realm) (printf "nfs/*@%s" $k.realm) -}}
{{- range $k.idmap }}{{ $list = append $list .principal }}{{ end }}
  value: {{ join "," $list | quote }}
{{- end }}
{{- $rules := list -}}
{{- range $k.idmap -}}
{{- $id := printf "%s=%v:%v" .principal .uid .gid -}}
{{- if .groups }}{{ $gs := list }}{{ range .groups }}{{ $gs = append $gs (printf "%v" .) }}{{ end }}{{ $id = printf "%s:%s" $id (join ";" $gs) }}{{ end -}}
{{- $rules = append $rules $id -}}
{{- end -}}
{{- $rules = append $rules (printf "host/*@%s=%s" $k.realm $k.machineIdentity) -}}
{{- $rules = append $rules (printf "nfs/*@%s=%s" $k.realm $k.machineIdentity) }}
- name: FLINT_NFS_GSS_IDMAP
  value: {{ join "," $rules | quote }}
- name: FLINT_NFS_GSS_ANON
  value: {{ $k.anonIdentity | quote }}
- name: FLINT_NFS_MIN_SEC
  value: {{ $k.minSec | quote }}
{{- if $k.enforcePermissions }}
- name: FLINT_NFS_ENFORCE_PERMISSIONS
  value: enforce
{{- end }}
{{- if $k.rootSquash }}
- name: FLINT_NFS_ROOT_SQUASH
  value: "true"
{{- end }}
{{- if $k.requireMachCred }}
- name: FLINT_NFS_REQUIRE_MACH_CRED
  value: "true"
{{- end }}
{{- if $l.enabled }}
- name: FLINT_NFS_GSS_LDAP_URL
  value: {{ $l.url | quote }}
{{- if $l.insecure }}
- name: FLINT_NFS_GSS_LDAP_INSECURE
  value: "true"
{{- end }}
- name: FLINT_NFS_GSS_LDAP_REALMS
  value: {{ join "," ($l.realms | default (list $k.realm)) | quote }}
- name: FLINT_NFS_GSS_LDAP_USER_BASE
  value: {{ $l.userBase | quote }}
{{- with $l.groupBase }}
- name: FLINT_NFS_GSS_LDAP_GROUP_BASE
  value: {{ . | quote }}
{{- end }}
{{- with $l.userFilter }}
- name: FLINT_NFS_GSS_LDAP_USER_FILTER
  value: {{ . | quote }}
{{- end }}
{{- with $l.principalAttribute }}
- name: FLINT_NFS_GSS_LDAP_PRINCIPAL_ATTR
  value: {{ . | quote }}
{{- end }}
{{- with $l.groupMembership }}
- name: FLINT_NFS_GSS_LDAP_GROUP_MEMBERSHIP
  value: {{ . | quote }}
{{- end }}
{{- with $l.bindDN }}
- name: FLINT_NFS_GSS_LDAP_BIND_DN
  value: {{ . | quote }}
- name: FLINT_NFS_GSS_LDAP_BIND_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ $l.bindPasswordSecret.name | quote }}
      key: {{ $l.bindPasswordSecret.key | default "password" | quote }}
      optional: false
{{- end }}
{{- if or $l.ca.configMap $l.ca.secret }}
- name: FLINT_NFS_GSS_LDAP_CA_FILE
  value: /etc/flint-ldap-ca/ca.crt
{{- end }}
{{- end }}
{{- end }}

{{- define "flint-lite.krb5Volumes" -}}
{{- $l := .Values.security.kerberos.ldap -}}
- name: krb5-keytab
  secret:
    secretName: flint-lite-krb5
    defaultMode: 292
{{- if and $l.enabled (or $l.ca.configMap $l.ca.secret) }}
- name: ldap-ca
  {{- if $l.ca.configMap }}
  configMap:
    name: {{ $l.ca.configMap }}
    items:
      - key: {{ $l.ca.key | default "ca.crt" }}
        path: ca.crt
    defaultMode: 292
  {{- else }}
  secret:
    secretName: {{ $l.ca.secret }}
    items:
      - key: {{ $l.ca.key | default "ca.crt" }}
        path: ca.crt
    defaultMode: 292
  {{- end }}
{{- end }}
{{- end }}

{{- define "flint-lite.krb5Mounts" -}}
{{- $l := .Values.security.kerberos.ldap -}}
- name: krb5-keytab
  mountPath: /etc/flint-krb5
  readOnly: true
{{- if and $l.enabled (or $l.ca.configMap $l.ca.secret) }}
- name: ldap-ca
  mountPath: /etc/flint-ldap-ca
  readOnly: true
{{- end }}
{{- end }}
