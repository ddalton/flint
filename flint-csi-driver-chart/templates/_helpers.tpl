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

{{/*
spdk_tgt's effective argument list.

TWO KNOBS USED TO DISAGREE. `spdkTarget.hugepages.enabled` controlled the
kubelet resource request and the /hugepages mount, but `--no-huge` was emitted
ONLY under kindMode. Setting hugepages.enabled=false on a real cluster
therefore took the hugepages away without telling SPDK, and DPDK's EAL init
fails at startup. This template makes the one knob do both.

`--no-huge` REQUIRES `-s <MB>` (SPDK lib/env_dpdk/init.c) and forces
--legacy-mem, so the heap cannot grow at runtime and the size is fixed here.
That memory is ordinary anonymous memory, so unlike hugepages — which are a
separate kubelet resource — it counts against the container's memory limit.
Keep spdkTarget.memory.limit comfortably above noHugeMemoryMB.

Safe only because flint's data paths never need physical addresses:
spdk_vtophys is called zero times in bdev_uring.c, lib/nvmf/tcp.c, lib/ublk/*
and lib/lvol/*. The kernel does the DMA. The one path that would need
hugepages back is the local-PCIe userspace NVMe driver
(bdev_nvme_attach_controller), which has never once attached on any cluster.

Returns a JSON array because the two call sites consume it differently: the
ublk branch shell-quotes it onto spdk-csi-start.sh's command line, the plain
branch renders it as a YAML list.
*/}}
{{- define "flint-csi-driver-chart.spdkTargetArgs" -}}
{{- $extra := .Values.spdkTarget.extraArgs | default list -}}
{{- if has "--no-huge" $extra -}}
{{- fail "spdkTarget.extraArgs contains --no-huge. Set spdkTarget.hugepages.enabled=false instead — that one switch emits --no-huge -s AND drops the hugepages resource request, which passing the flag by hand does not." -}}
{{- end -}}
{{- $args := list -}}
{{- if .Values.spdkTarget.kindMode.enabled -}}
{{- $args = concat $args (list "--no-huge" "-s" (.Values.spdkTarget.kindMode.spdkMemoryMB | toString) "--no-pci") -}}
{{- if ne (toString .Values.spdkTarget.kindMode.interruptMode) "false" -}}
{{- $args = append $args "--interrupt-mode" -}}
{{- end -}}
{{- else if not .Values.spdkTarget.hugepages.enabled -}}
{{- $args = concat $args (list "--no-huge" "-s" (.Values.spdkTarget.hugepages.noHugeMemoryMB | default 1024 | toString)) -}}
{{- end -}}
{{- $args = concat $args $extra -}}
{{- toJson $args -}}
{{- end -}}

{{/* "<registry>/" or "" — global.imageRegistry overrides the image's own
     registry; empty means the repository's implicit registry (Docker Hub).
     Called with (dict "root" $ "registry" …). */}}
{{- define "flint-csi-driver-chart.registryPrefix" -}}
{{- with (coalesce .root.Values.global.imageRegistry .registry) -}}{{ . }}/{{- end -}}
{{- end }}

{{/* [registry/]images.repository — the prefix every flint image is named
     under, and what the controller receives as IMAGE_REPOSITORY. */}}
{{- define "flint-csi-driver-chart.imagePrefix" -}}
{{- include "flint-csi-driver-chart.registryPrefix" (dict "root" . "registry" .Values.images.registry) }}{{ .Values.images.repository }}
{{- end }}

{{/* [registry/]csiSidecars.repository */}}
{{- define "flint-csi-driver-chart.sidecarPrefix" -}}
{{- include "flint-csi-driver-chart.registryPrefix" (dict "root" . "registry" .Values.csiSidecars.registry) }}{{ .Values.csiSidecars.repository }}
{{- end }}

{{/*
Kerberos for RWX NFS (nfs.kerberos): the name every NFS server goes by, the
NFS namespace, the servers' keytab Secret.
*/}}
{{- define "flint-csi-driver-chart.nfsNamespace" -}}
{{- .Values.nfs.namespace | default .Release.Namespace -}}
{{- end }}

{{- define "flint-csi-driver-chart.nfsKrb5ServerName" -}}
{{- .Values.nfs.kerberos.serverName | default (printf "flint-nfs.%s.svc.%s" (include "flint-csi-driver-chart.nfsNamespace" .) .Values.nfs.kerberos.clusterDomain) -}}
{{- end }}

{{- define "flint-csi-driver-chart.nfsKrb5Principals" -}}
{{- $k := .Values.nfs.kerberos -}}
{{- if $k.allowedPrincipals -}}
{{- join "," $k.allowedPrincipals -}}
{{- else -}}
{{- $list := list (printf "host/*@%s" $k.realm) (printf "nfs/*@%s" $k.realm) -}}
{{- range $k.idmap }}{{ $list = append $list .principal }}{{ end -}}
{{- join "," $list -}}
{{- end -}}
{{- end }}

{{/*
FLINT_NFS_GSS_IDMAP: the user rules, then the machine principals.
*/}}
{{- define "flint-csi-driver-chart.nfsKrb5Idmap" -}}
{{- $k := .Values.nfs.kerberos -}}
{{- $rules := list -}}
{{- range $k.idmap -}}
{{- $id := printf "%s=%v:%v" (required "nfs.kerberos.idmap[].principal" .principal) (required "nfs.kerberos.idmap[].uid" .uid) (required "nfs.kerberos.idmap[].gid" .gid) -}}
{{- if .groups }}{{ $id = printf "%s:%s" $id (join ";" .groups) }}{{ end -}}
{{- $rules = append $rules $id -}}
{{- end -}}
{{- $rules = append $rules (printf "host/*@%s=%s" $k.realm $k.machineIdentity) -}}
{{- $rules = append $rules (printf "nfs/*@%s=%s" $k.realm $k.machineIdentity) -}}
{{- join "," $rules -}}
{{- end }}

{{/*
The directory lookup (nfs.kerberos.ldap): the controller's NFS_GSS_LDAP_*,
which every NFS server gets as FLINT_NFS_GSS_LDAP_*. The bind password and
the CA bundle are named, not copied: the servers read them from the Secret
and ConfigMap.
*/}}
{{- define "flint-csi-driver-chart.nfsKrb5LdapEnv" -}}
{{- include "flint-csi-driver-chart.krb5LdapEnv" (dict "root" . "server" false) -}}
{{- end }}

{{/*
The directory lookup's env, for the controller (server: false, NFS_GSS_LDAP_*,
the password Secret and CA by name, which nfs_krb5::server_pod_parts turns
into the RWX servers' env) or for a server pod the chart makes itself
(server: true, the pNFS MDS: FLINT_NFS_GSS_LDAP_*, the password as a
secretKeyRef, the CA as a file from flint-csi-driver-chart.krb5ServerVolumes).
*/}}
{{- define "flint-csi-driver-chart.krb5LdapEnv" -}}
{{- $root := .root -}}
{{- $server := .server -}}
{{- $p := ternary "FLINT_NFS_GSS_LDAP_" "NFS_GSS_LDAP_" $server -}}
{{- $k := $root.Values.nfs.kerberos -}}
{{- $l := $k.ldap -}}
{{- if $l.enabled }}
{{- $url := required "nfs.kerberos.ldap.url" $l.url -}}
{{- if not (or (hasPrefix "ldaps://" $url) (hasPrefix "ldap://" $url)) -}}
{{- fail "nfs.kerberos.ldap.url: ldaps://host or ldap://host (StartTLS)" -}}
{{- end -}}
{{- if and $l.bindDN (not $l.bindPasswordSecret.name) -}}
{{- fail "nfs.kerberos.ldap.bindDN needs bindPasswordSecret.name" -}}
{{- end -}}
{{- if and $l.bindPasswordSecret.name (not $l.bindDN) -}}
{{- fail "nfs.kerberos.ldap.bindPasswordSecret needs bindDN" -}}
{{- end -}}
{{- if and $l.ca.configMap $l.ca.secret -}}
{{- fail "nfs.kerberos.ldap.ca: a configMap or a secret, not both" -}}
{{- end }}
- name: {{ $p }}URL
  value: {{ $url | quote }}
{{- if $l.insecure }}
- name: {{ $p }}INSECURE
  value: "true"
{{- end }}
- name: {{ $p }}REALMS
  value: {{ join "," ($l.realms | default (list $k.realm)) | quote }}
- name: {{ $p }}USER_BASE
  value: {{ required "nfs.kerberos.ldap.userBase" $l.userBase | quote }}
{{- with $l.groupBase }}
- name: {{ $p }}GROUP_BASE
  value: {{ . | quote }}
{{- end }}
{{- with $l.userFilter }}
- name: {{ $p }}USER_FILTER
  value: {{ . | quote }}
{{- end }}
{{- with $l.principalAttribute }}
- name: {{ $p }}PRINCIPAL_ATTR
  value: {{ . | quote }}
{{- end }}
{{- with $l.groupMembership }}
- name: {{ $p }}GROUP_MEMBERSHIP
  value: {{ . | quote }}
{{- end }}
{{- with $l.bindDN }}
- name: {{ $p }}BIND_DN
  value: {{ . | quote }}
{{- if $server }}
- name: {{ $p }}BIND_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ $l.bindPasswordSecret.name | quote }}
      key: {{ $l.bindPasswordSecret.key | default "password" | quote }}
      optional: false
{{- else }}
- name: {{ $p }}BIND_PASSWORD_SECRET
  value: {{ $l.bindPasswordSecret.name | quote }}
- name: {{ $p }}BIND_PASSWORD_KEY
  value: {{ $l.bindPasswordSecret.key | default "password" | quote }}
{{- end }}
{{- end }}
{{- if $server }}
{{- if or $l.ca.configMap $l.ca.secret }}
- name: {{ $p }}CA_FILE
  value: /etc/flint-ldap-ca/ca.crt
{{- end }}
{{- else }}
{{- with $l.ca.configMap }}
- name: {{ $p }}CA_CONFIGMAP
  value: {{ . | quote }}
{{- end }}
{{- with $l.ca.secret }}
- name: {{ $p }}CA_SECRET
  value: {{ . | quote }}
{{- end }}
{{- if or $l.ca.configMap $l.ca.secret }}
- name: {{ $p }}CA_KEY
  value: {{ $l.ca.key | default "ca.crt" | quote }}
{{- end }}
{{- end }}
- name: {{ $p }}TTL
  value: {{ $l.cacheTTL | toString | quote }}
- name: {{ $p }}NEGATIVE_TTL
  value: {{ $l.negativeCacheTTL | toString | quote }}
- name: {{ $p }}TIMEOUT
  value: {{ $l.timeout | toString | quote }}
- name: {{ $p }}MAX_CONCURRENT
  value: {{ $l.maxConcurrent | toString | quote }}
{{- if $l.allowRoot }}
- name: {{ $p }}ALLOW_ROOT
  value: "true"
{{- end }}
{{- end }}
{{- end }}

{{/*
FLINT_NFS_ENFORCE_PERMISSIONS: as set, else "enforce" under Kerberos (whose
callers' identities are proven), else the server's default.
*/}}
{{- define "flint-csi-driver-chart.nfsEnforcePermissions" -}}
{{- .Values.nfs.enforcePermissions | default (ternary "enforce" "" .Values.nfs.kerberos.enabled) -}}
{{- end }}

{{- define "flint-csi-driver-chart.nfsKrb5KeytabSecret" -}}
flint-nfs-krb5-keytab
{{- end }}

{{- define "flint-csi-driver-chart.nfsKrb5NodeEnv" -}}
{{- if and .Values.nfs.enabled .Values.nfs.kerberos.enabled }}
- name: FLINT_NFS_KRB5_SERVER_NAME
  value: {{ include "flint-csi-driver-chart.nfsKrb5ServerName" . | quote }}
- name: FLINT_NFS_KRB5_HOSTS_FILE
  value: /host/etc/hosts
{{- else if include "flint-csi-driver-chart.pnfsKrb5Enabled" . }}
# pNFS Kerberos: the node maps each MDS shard's IP to its name here.
- name: FLINT_NFS_KRB5_HOSTS_FILE
  value: /host/etc/hosts
{{- end }}
{{- if include "flint-csi-driver-chart.pnfsKrb5DataServers" . }}
# And each data server's Services (its own, its multipath ones) to the
# name its principal is for (nfs_krb5::PNFS_DS_ENV).
- name: FLINT_PNFS_KRB5_DS
  value: {{ include "flint-csi-driver-chart.pnfsKrb5DsNames" . | quote }}
{{- end }}
{{- end }}

{{/*
pnfs.kerberos.dataServers in effect (Phase 3): the data servers do
Kerberos, so Kerberos callers get layouts.
*/}}
{{/*
The data servers' attached-nodes gate: they do Kerberos and
pnfs.kerberos.attachedNodesOnly is on.
*/}}
{{- define "flint-csi-driver-chart.pnfsDsAttachedNodesOnly" -}}
{{- if and (include "flint-csi-driver-chart.pnfsKrb5DataServers" .) (include "flint-csi-driver-chart.pnfsAttachedNodesOnly" .) -}}true{{- end -}}
{{- end }}

{{/*
TLS on the pNFS control path (pnfs.controlTls, Phase 5): "true", else empty.
*/}}
{{- define "flint-csi-driver-chart.pnfsControlTls" -}}
{{- if and .Values.pnfs.enabled .Values.pnfs.server.enabled (.Values.pnfs.controlTls).enabled -}}true{{- end -}}
{{- end }}

{{- define "flint-csi-driver-chart.pnfsControlTlsEnv" -}}
- name: FLINT_PNFS_CONTROL_TLS_DIR
  value: /etc/flint-pnfs-control-tls
{{- end }}

{{- define "flint-csi-driver-chart.pnfsControlTlsMount" -}}
- name: pnfs-control-tls
  mountPath: /etc/flint-pnfs-control-tls
  readOnly: true
{{- end }}

{{- define "flint-csi-driver-chart.pnfsControlTlsVolume" -}}
- name: pnfs-control-tls
  secret:
    secretName: {{ .Values.pnfs.controlTls.existingSecret | default "flint-pnfs-control-tls" }}
    defaultMode: 0444
{{- end }}

{{- define "flint-csi-driver-chart.pnfsKrb5DataServers" -}}
{{- if and (include "flint-csi-driver-chart.pnfsKrb5Enabled" .) (.Values.pnfs.kerberos).dataServers -}}true{{- end -}}
{{- end }}

{{/*
FLINT_PNFS_KRB5_DS: <service>=<principal host> for every data server
Service. The principal host is the pod's DNS name under the StatefulSet's
headless Service, as krb5-operator's hostPrincipals name it.
*/}}
{{- define "flint-csi-driver-chart.pnfsKrb5DsNames" -}}
{{- $base := include "flint-csi-driver-chart.pnfsKrb5NameBase" . -}}
{{- $mp := int (.Values.pnfs.server.dataServers.multipathServices | default 0) -}}
{{- $out := list -}}
{{- range $i := until (int .Values.pnfs.server.dataServers.count) -}}
{{- $host := printf "flint-pnfs-ds-%d.flint-pnfs-ds-headless.%s" $i $base -}}
{{- $out = append $out (printf "flint-pnfs-ds-%d.%s=%s" $i $base $host) -}}
{{- range $j := until $mp -}}
{{- $out = append $out (printf "flint-pnfs-ds-%d-mp%d.%s=%s" $i (add1 $j) $base $host) -}}
{{- end -}}
{{- end -}}
{{- join "," $out -}}
{{- end }}

{{- define "flint-csi-driver-chart.nfsKrb5NodeMounts" -}}
{{- if or (and .Values.nfs.enabled .Values.nfs.kerberos.enabled) (include "flint-csi-driver-chart.pnfsKrb5Enabled" .) }}
- name: host-etc-hosts
  mountPath: /host/etc/hosts
{{- end }}
{{- end }}

{{/*
pNFS Kerberos (pnfs.kerberos) is on: "true", else empty. It reuses
nfs.kerberos's realm, allowlist, idmap, directory and node keys, whether or
not RWX Kerberos (nfs.kerberos.enabled) is on.
*/}}
{{- define "flint-csi-driver-chart.pnfsKrb5Enabled" -}}
{{- if and .Values.pnfs.enabled (.Values.pnfs.kerberos).enabled -}}true{{- end -}}
{{- end }}

{{/*
pnfs.kerberos.attachedNodesOnly in effect: the MDS watches attachments and
needs its service account.
*/}}
{{- define "flint-csi-driver-chart.pnfsAttachedNodesOnly" -}}
{{- if and (include "flint-csi-driver-chart.pnfsKrb5Enabled" .) (.Values.pnfs.kerberos).attachedNodesOnly -}}true{{- end -}}
{{- end }}

{{/*
The DNS suffix of the MDS shard Services (pnfs-mds.yaml), under which each
shard's Kerberos name is flint-pnfs-mds-<shard>.<this>. The controller's
PNFS_KRB5_MDS_NAME_BASE; the principals in pnfs-kerberos.yaml.
*/}}
{{- define "flint-csi-driver-chart.pnfsKrb5NameBase" -}}
{{- printf "%s.svc.%s" .Release.Namespace .Values.nfs.kerberos.clusterDomain -}}
{{- end }}

{{/*
KerberosNodeKeytab: the nodes' machine credentials for rpc.gssd. Rendered
by nfs-kerberos.yaml when RWX Kerberos is on, else by pnfs-kerberos.yaml
when pNFS Kerberos is on (flint-csi-driver-chart.krb5NodeKeytabByRwx says
which), so it is never rendered twice.
*/}}
{{- define "flint-csi-driver-chart.krb5NodeKeytabByRwx" -}}
{{- if and (not .Values.lite.enabled) .Values.nfs.enabled .Values.nfs.kerberos.enabled -}}true{{- end -}}
{{- end }}

{{- define "flint-csi-driver-chart.krb5NodeKeytab" -}}
{{- $k := .Values.nfs.kerberos -}}
# The nodes' machine credentials for rpc.gssd (host/ and nfs/<node>), in
# their /etc/krb5.keytab, with the realm's krb5.conf.
apiVersion: krb5.ddalton.dev/v1alpha1
kind: KerberosNodeKeytab
metadata:
  name: {{ include "flint-csi-driver-chart.fullname" . | trunc 30 | trimSuffix "-" }}-nfs-clients
  labels:
    {{- include "flint-csi-driver-chart.labels" . | nindent 4 }}
spec:
  realmRef: {{ $k.realmRef }}
  realmNamespace: {{ $k.realmNamespace | default .Release.Namespace }}
  services: [host, nfs]
  {{- with $k.nodeKeytab.nodeSelector }}
  nodeSelector:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with $k.nodeKeytab.tolerations }}
  tolerations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with $k.nodeKeytab.domain }}
  domain: {{ . }}
  {{- end }}
  keytabPath: {{ $k.nodeKeytab.keytabPath }}
  {{- with $k.nodeKeytab.krb5ConfPath }}
  krb5ConfPath: {{ . }}
  {{- end }}
  {{- with $k.nodeKeytab.restartServices }}
  restartServices:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  deletionPolicy: {{ $k.deletionPolicy }}
{{- end }}

{{/*
A Kerberos NFS server pod's security env, for the server pods the chart
makes itself (the pNFS MDS). The same settings, by the same names, as
nfs_krb5::server_pod_parts gives the RWX servers from the controller's
NFS_* env: tests/system/krb5-server-env-names.txt lists them, and both a
unit test (nfs_krb5.rs) and tests/system/chart-kerberos-defaults.sh check
it. Change all three together.

Arguments: root, minSec, requireMachCred, enforcePermissions. The keytab is
at /etc/flint-krb5/krb5.keytab (flint-csi-driver-chart.krb5ServerVolumes):
not under /etc/flint, which is the MDS's ConfigMap, a read-only volume
that must not hold other volumes' mount points.
*/}}
{{- define "flint-csi-driver-chart.krb5ServerEnv" -}}
{{- $root := .root -}}
{{- $k := $root.Values.nfs.kerberos -}}
- name: KRB5_KTNAME
  value: /etc/flint-krb5/krb5.keytab
- name: FLINT_NFS_GSS_PRINCIPALS
  value: {{ include "flint-csi-driver-chart.nfsKrb5Principals" $root | quote }}
- name: FLINT_NFS_GSS_IDMAP
  value: {{ include "flint-csi-driver-chart.nfsKrb5Idmap" $root | quote }}
- name: FLINT_NFS_GSS_ANON
  value: {{ $k.anonIdentity | quote }}
{{- with .minSec }}
- name: FLINT_NFS_MIN_SEC
  value: {{ . | quote }}
{{- end }}
{{- with .enforcePermissions }}
- name: FLINT_NFS_ENFORCE_PERMISSIONS
  value: {{ . | quote }}
{{- end }}
{{- if $root.Values.nfs.rootSquash }}
- name: FLINT_NFS_ROOT_SQUASH
  value: "true"
{{- end }}
{{- if .requireMachCred }}
- name: FLINT_NFS_REQUIRE_MACH_CRED
  value: "true"
{{- end }}
{{- with include "flint-csi-driver-chart.krb5LdapEnv" (dict "root" $root "server" true) }}
{{ . | trim }}
{{- end }}
{{- end }}

{{/*
The volumes a Kerberos server pod needs (arguments: root, keytabSecret):
its keytab, and the directory's CA bundle when there is one. Mounted by
flint-csi-driver-chart.krb5ServerMounts.
*/}}
{{- define "flint-csi-driver-chart.krb5ServerVolumes" -}}
{{- $l := .root.Values.nfs.kerberos.ldap -}}
- name: krb5-keytab
  secret:
    secretName: {{ .keytabSecret }}
    # The server may run as non-root; the Secret's files are root's.
    defaultMode: 0444
{{- if and $l.enabled (or $l.ca.configMap $l.ca.secret) }}
- name: ldap-ca
  {{- if $l.ca.configMap }}
  configMap:
    name: {{ $l.ca.configMap }}
  {{- else }}
  secret:
    secretName: {{ $l.ca.secret }}
  {{- end }}
    items:
    - key: {{ $l.ca.key | default "ca.crt" }}
      path: ca.crt
    defaultMode: 0444
{{- end }}
{{- end }}

{{- define "flint-csi-driver-chart.krb5ServerMounts" -}}
{{- $l := .root.Values.nfs.kerberos.ldap -}}
{{- if .keytabSubPathExpr }}
# One file of the keytab Secret: this pod's (hostPrincipals).
- name: krb5-keytab
  mountPath: /etc/flint-krb5/krb5.keytab
  subPathExpr: {{ .keytabSubPathExpr }}
  readOnly: true
{{- else }}
- name: krb5-keytab
  mountPath: /etc/flint-krb5
  readOnly: true
{{- end }}
{{- if and $l.enabled (or $l.ca.configMap $l.ca.secret) }}
- name: ldap-ca
  mountPath: /etc/flint-ldap-ca
  readOnly: true
{{- end }}
{{- end }}
