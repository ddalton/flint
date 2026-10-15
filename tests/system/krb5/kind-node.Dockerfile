# A kind node that can mount NFS with Kerberos: nfs-common (rpc.gssd) and
# the MIT client tools. kind-nfs-only.sh builds it with KRB5=1; the node's
# keytab and krb5.conf come from krb5-operator (KerberosNodeKeytab).
ARG KIND_NODE_IMAGE=kindest/node:v1.34.0
FROM ${KIND_NODE_IMAGE}
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends nfs-common krb5-user \
 && rm -rf /var/lib/apt/lists/*
