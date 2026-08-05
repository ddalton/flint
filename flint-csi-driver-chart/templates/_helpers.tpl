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
{{- $args = concat $args (list "--no-huge" "-s" (.Values.spdkTarget.kindMode.spdkMemoryMB | toString) "--no-pci" "--interrupt-mode") -}}
{{- else if not .Values.spdkTarget.hugepages.enabled -}}
{{- $args = concat $args (list "--no-huge" "-s" (.Values.spdkTarget.hugepages.noHugeMemoryMB | default 1024 | toString)) -}}
{{- end -}}
{{- $args = concat $args $extra -}}
{{- toJson $args -}}
{{- end -}}
