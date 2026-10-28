// coshim stands in for the Kubernetes CO between csi-sanity and the flint
// controller, so the sanity suite reaches the code paths that read the PV.
//
// flint keeps a volume's placement in its PV (spec.csi.volumeAttributes =
// the CreateVolume volume_context); in a cluster the external-provisioner
// writes that PV right after CreateVolume returns. csi-sanity creates no
// PV, so every later RPC that needs one (ControllerPublish, the snapshot
// and expand paths, NodeStage's restage) failed NOT_FOUND before testing
// anything. coshim is a transparent gRPC proxy that does the
// provisioner's two jobs and nothing else:
//
//   - CreateVolume: the name becomes `pvc-<uuid>`, derived from the CO's
//     name (as the provisioner names PVs; csi-sanity's names end in
//     uppercase hex, which no PV name may hold). An empty name is passed
//     through, so the driver's own refusal is what csi-sanity sees. On
//     success the PV is written from the response.
//   - DeleteVolume: on success the PV is deleted.
//   - ControllerPublishVolume / ControllerUnpublishVolume: on success a
//     VolumeAttachment for (PV, node) is written as attached, or deleted.
//     The node reconcile derives each export's allowed hosts from it, so
//     without one a consumer was fenced off its own volume's legs mid-spec
//     (a raid base dropped, the leg went stale). The attachment names its
//     own attacher, so the driver's external-attacher sidecar leaves it be.
//
// Every other call is forwarded byte for byte. With -map-names=false the
// names are forwarded as they come; csi-sanity's maximum-length test is
// only meaningful in that mode, since a mapped name is always 40 bytes.
package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net"
	"os/exec"
	"strconv"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/mem"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/proto"
)

// rawCodec moves message bytes without decoding them.
type rawCodec struct{}

func (rawCodec) Marshal(v any) (mem.BufferSlice, error) {
	return mem.BufferSlice{mem.SliceBuffer(*(v.(*[]byte)))}, nil
}

func (rawCodec) Unmarshal(data mem.BufferSlice, v any) error {
	*(v.(*[]byte)) = data.Materialize()
	return nil
}

func (rawCodec) Name() string { return "proto" }

var (
	listen     = flag.String("listen", "127.0.0.1:10001", "address csi-sanity dials as its controller endpoint")
	controller = flag.String("controller", "", "the flint controller's gRPC address (the chart's sanity proxy, pod IP:10000)")
	driver     = flag.String("driver", "disk.csi.chert.us", "CSI driver name written into each PV")
	kubectl    = flag.String("kubectl", "kubectl", "kubectl binary")
	kubeconfig = flag.String("kubeconfig", "", "kubeconfig allowed to create and delete PVs")
	mapNames   = flag.Bool("map-names", true, "rename volumes to pvc-<uuid> as the provisioner does")
)

const (
	createVolume      = "/csi.v1.Controller/CreateVolume"
	deleteVolume      = "/csi.v1.Controller/DeleteVolume"
	controllerPublish = "/csi.v1.Controller/ControllerPublishVolume"
	controllerUnpub   = "/csi.v1.Controller/ControllerUnpublishVolume"
	// Not the driver's name: the external-attacher acts on attachments that
	// name its driver, and would call ControllerPublish/Unpublish itself.
	attacherName = "coshim.sanity"
)

// pvName is the provisioner-shaped name for a CO name: deterministic, so a
// repeated CreateVolume (the idempotency specs) maps to the same volume.
func pvName(name string) string {
	h := sha256.Sum256([]byte(name))
	x := fmt.Sprintf("%x", h[:16])
	return fmt.Sprintf("pvc-%s-%s-%s-%s-%s", x[0:8], x[8:12], x[12:16], x[16:20], x[20:32])
}

func main() {
	flag.Parse()
	if *controller == "" {
		log.Fatal("-controller is required")
	}
	up, err := grpc.NewClient(*controller, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		log.Fatalf("dial %s: %v", *controller, err)
	}
	lis, err := net.Listen("tcp", *listen)
	if err != nil {
		log.Fatalf("listen %s: %v", *listen, err)
	}
	srv := grpc.NewServer(
		grpc.ForceServerCodecV2(rawCodec{}),
		grpc.UnknownServiceHandler(func(_ any, ss grpc.ServerStream) error {
			return forward(up, ss)
		}),
	)
	log.Printf("coshim: %s -> %s (map-names=%v)", *listen, *controller, *mapNames)
	log.Fatal(srv.Serve(lis))
}

// forward relays one unary call (every CSI RPC is unary), rewriting the
// two the provisioner owns.
func forward(up *grpc.ClientConn, ss grpc.ServerStream) error {
	method, _ := grpc.MethodFromServerStream(ss)
	var req []byte
	if err := ss.RecvMsg(&req); err != nil {
		return err
	}
	ctx := ss.Context()
	if md, ok := metadata.FromIncomingContext(ctx); ok {
		ctx = metadata.NewOutgoingContext(ctx, md)
	}
	var createReq *csi.CreateVolumeRequest
	if method == createVolume {
		createReq = &csi.CreateVolumeRequest{}
		if err := proto.Unmarshal(req, createReq); err != nil {
			return status.Errorf(codes.Internal, "coshim: decode CreateVolume: %v", err)
		}
		if *mapNames && createReq.Name != "" {
			mapped := pvName(createReq.Name)
			log.Printf("CreateVolume %q -> %s", createReq.Name, mapped)
			createReq.Name = mapped
			b, err := proto.Marshal(createReq)
			if err != nil {
				return status.Errorf(codes.Internal, "coshim: encode CreateVolume: %v", err)
			}
			req = b
		}
	}
	var resp []byte
	if err := up.Invoke(ctx, method, &req, &resp, grpc.ForceCodecV2(rawCodec{})); err != nil {
		return err
	}
	switch method {
	case createVolume:
		out := &csi.CreateVolumeResponse{}
		if err := proto.Unmarshal(resp, out); err != nil {
			return status.Errorf(codes.Internal, "coshim: decode CreateVolume response: %v", err)
		}
		if err := writePV(createReq, out.GetVolume()); err != nil {
			return status.Errorf(codes.Internal, "coshim: volume %s was created but its PV was not: %v",
				out.GetVolume().GetVolumeId(), err)
		}
	case controllerPublish:
		in := &csi.ControllerPublishVolumeRequest{}
		if err := proto.Unmarshal(req, in); err == nil {
			if err := writeVA(in.VolumeId, in.NodeId); err != nil {
				return status.Errorf(codes.Internal, "coshim: volume %s was published to %s but its VolumeAttachment was not: %v",
					in.VolumeId, in.NodeId, err)
			}
		}
	case controllerUnpub:
		in := &csi.ControllerUnpublishVolumeRequest{}
		if err := proto.Unmarshal(req, in); err == nil && in.VolumeId != "" {
			sel := "coshim/volume=" + in.VolumeId
			if in.NodeId != "" {
				sel += ",coshim/node=" + in.NodeId
			}
			if err := run(nil, "delete", "volumeattachment", "-l", sel, "--ignore-not-found", "--wait=false"); err != nil {
				return status.Errorf(codes.Internal, "coshim: volume %s was unpublished but its VolumeAttachment was not deleted: %v", in.VolumeId, err)
			}
			log.Printf("ControllerUnpublish %s from %q: VolumeAttachment deleted", in.VolumeId, in.NodeId)
		}
	case deleteVolume:
		in := &csi.DeleteVolumeRequest{}
		if err := proto.Unmarshal(req, in); err == nil && in.VolumeId != "" {
			if err := run(nil, "delete", "pv", in.VolumeId, "--ignore-not-found", "--wait=false"); err != nil {
				return status.Errorf(codes.Internal, "coshim: volume %s was deleted but its PV was not: %v", in.VolumeId, err)
			}
			log.Printf("DeleteVolume %s: PV deleted", in.VolumeId)
		}
	}
	return ss.SendMsg(&resp)
}

// accessMode is the external-provisioner's CSI -> Kubernetes mapping.
func accessMode(m csi.VolumeCapability_AccessMode_Mode) string {
	switch m {
	case csi.VolumeCapability_AccessMode_MULTI_NODE_READER_ONLY:
		return "ReadOnlyMany"
	case csi.VolumeCapability_AccessMode_MULTI_NODE_SINGLE_WRITER, csi.VolumeCapability_AccessMode_MULTI_NODE_MULTI_WRITER:
		return "ReadWriteMany"
	case csi.VolumeCapability_AccessMode_SINGLE_NODE_SINGLE_WRITER:
		return "ReadWriteOncePod"
	default:
		return "ReadWriteOnce"
	}
}

// writePV writes the PV the external-provisioner would: named for the
// volume id, the volume_context as volumeAttributes, Retain so nothing in
// the cluster acts on it once csi-sanity is done.
func writePV(req *csi.CreateVolumeRequest, vol *csi.Volume) error {
	if vol == nil || vol.VolumeId == "" {
		return fmt.Errorf("the response carries no volume")
	}
	modes := []string{}
	seen := map[string]bool{}
	volumeMode, fsType := "Filesystem", ""
	for _, c := range req.GetVolumeCapabilities() {
		if m := accessMode(c.GetAccessMode().GetMode()); !seen[m] {
			seen[m] = true
			modes = append(modes, m)
		}
		if c.GetBlock() != nil {
			volumeMode = "Block"
		}
		if fs := c.GetMount().GetFsType(); fs != "" {
			fsType = fs
		}
	}
	csiSrc := map[string]any{
		"driver":           *driver,
		"volumeHandle":     vol.VolumeId,
		"volumeAttributes": vol.VolumeContext,
	}
	if fsType != "" {
		csiSrc["fsType"] = fsType
	}
	spec := map[string]any{
		"capacity":                      map[string]string{"storage": strconv.FormatInt(vol.CapacityBytes, 10)},
		"accessModes":                   modes,
		"persistentVolumeReclaimPolicy": "Retain",
		"volumeMode":                    volumeMode,
		"csi":                           csiSrc,
	}
	if len(vol.AccessibleTopology) > 0 {
		terms := []any{}
		for _, t := range vol.AccessibleTopology {
			exprs := []any{}
			for k, v := range t.Segments {
				exprs = append(exprs, map[string]any{"key": k, "operator": "In", "values": []string{v}})
			}
			terms = append(terms, map[string]any{"matchExpressions": exprs})
		}
		spec["nodeAffinity"] = map[string]any{"required": map[string]any{"nodeSelectorTerms": terms}}
	}
	pv := map[string]any{
		"apiVersion": "v1",
		"kind":       "PersistentVolume",
		"metadata": map[string]any{
			"name":   vol.VolumeId,
			"labels": map[string]string{"app.kubernetes.io/managed-by": "coshim"},
		},
		"spec": spec,
	}
	body, err := json.Marshal(pv)
	if err != nil {
		return err
	}
	if err := run(body, "apply", "-f", "-"); err != nil {
		return err
	}
	log.Printf("CreateVolume %s: PV written (%d bytes, %v, %s)", vol.VolumeId, vol.CapacityBytes, modes, volumeMode)
	return nil
}

// writeVA writes the attached VolumeAttachment the attach/detach controller
// and external-attacher would leave for (volume, node), named as Kubernetes
// names them. Labels carry the pair (node names fit a label value) so the
// unpublish can find it.
func writeVA(volumeID, nodeID string) error {
	h := sha256.Sum256([]byte(volumeID + attacherName + nodeID))
	name := fmt.Sprintf("csi-%x", h)
	va := map[string]any{
		"apiVersion": "storage.k8s.io/v1",
		"kind":       "VolumeAttachment",
		"metadata": map[string]any{
			"name": name,
			"labels": map[string]string{
				"app.kubernetes.io/managed-by": "coshim",
				"coshim/volume":                volumeID,
				"coshim/node":                  nodeID,
			},
		},
		"spec": map[string]any{
			"attacher": attacherName,
			"nodeName": nodeID,
			"source":   map[string]string{"persistentVolumeName": volumeID},
		},
	}
	body, err := json.Marshal(va)
	if err != nil {
		return err
	}
	if err := run(body, "apply", "-f", "-"); err != nil {
		return err
	}
	if err := run(nil, "patch", "volumeattachment", name, "--subresource=status", "--type=merge",
		"-p", `{"status":{"attached":true}}`); err != nil {
		return err
	}
	log.Printf("ControllerPublish %s to %s: VolumeAttachment %s attached", volumeID, nodeID, name)
	return nil
}

func run(stdin []byte, args ...string) error {
	if *kubeconfig != "" {
		args = append([]string{"--kubeconfig", *kubeconfig}, args...)
	}
	cmd := exec.CommandContext(context.Background(), *kubectl, args...)
	if stdin != nil {
		cmd.Stdin = bytes.NewReader(stdin)
	}
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("kubectl %v: %v: %s", args, err, bytes.TrimSpace(out))
	}
	return nil
}
