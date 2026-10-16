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
{{- $k := .Values.nfs.kerberos -}}
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
- name: NFS_GSS_LDAP_URL
  value: {{ $url | quote }}
{{- if $l.insecure }}
- name: NFS_GSS_LDAP_INSECURE
  value: "true"
{{- end }}
- name: NFS_GSS_LDAP_REALMS
  value: {{ join "," ($l.realms | default (list $k.realm)) | quote }}
- name: NFS_GSS_LDAP_USER_BASE
  value: {{ required "nfs.kerberos.ldap.userBase" $l.userBase | quote }}
{{- with $l.groupBase }}
- name: NFS_GSS_LDAP_GROUP_BASE
  value: {{ . | quote }}
{{- end }}
{{- with $l.userFilter }}
- name: NFS_GSS_LDAP_USER_FILTER
  value: {{ . | quote }}
{{- end }}
{{- with $l.principalAttribute }}
- name: NFS_GSS_LDAP_PRINCIPAL_ATTR
  value: {{ . | quote }}
{{- end }}
{{- with $l.groupMembership }}
- name: NFS_GSS_LDAP_GROUP_MEMBERSHIP
  value: {{ . | quote }}
{{- end }}
{{- with $l.bindDN }}
- name: NFS_GSS_LDAP_BIND_DN
  value: {{ . | quote }}
- name: NFS_GSS_LDAP_BIND_PASSWORD_SECRET
  value: {{ $l.bindPasswordSecret.name | quote }}
- name: NFS_GSS_LDAP_BIND_PASSWORD_KEY
  value: {{ $l.bindPasswordSecret.key | default "password" | quote }}
{{- end }}
{{- with $l.ca.configMap }}
- name: NFS_GSS_LDAP_CA_CONFIGMAP
  value: {{ . | quote }}
{{- end }}
{{- with $l.ca.secret }}
- name: NFS_GSS_LDAP_CA_SECRET
  value: {{ . | quote }}
{{- end }}
{{- if or $l.ca.configMap $l.ca.secret }}
- name: NFS_GSS_LDAP_CA_KEY
  value: {{ $l.ca.key | default "ca.crt" | quote }}
{{- end }}
- name: NFS_GSS_LDAP_TTL
  value: {{ $l.cacheTTL | toString | quote }}
- name: NFS_GSS_LDAP_NEGATIVE_TTL
  value: {{ $l.negativeCacheTTL | toString | quote }}
- name: NFS_GSS_LDAP_TIMEOUT
  value: {{ $l.timeout | toString | quote }}
- name: NFS_GSS_LDAP_MAX_CONCURRENT
  value: {{ $l.maxConcurrent | toString | quote }}
{{- if $l.allowRoot }}
- name: NFS_GSS_LDAP_ALLOW_ROOT
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
{{- end }}
{{- end }}

{{- define "flint-csi-driver-chart.nfsKrb5NodeMounts" -}}
{{- if and .Values.nfs.enabled .Values.nfs.kerberos.enabled }}
- name: host-etc-hosts
  mountPath: /host/etc/hosts
{{- end }}
{{- end }}
