#!/bin/bash
set -euo pipefail
REALM=FLINT.TEST
PW=flintflint

sudo tee /etc/krb5.conf >/dev/null <<CONF
[libdefaults]
    default_realm = $REALM
    dns_lookup_realm = false
    dns_lookup_kdc = false
    rdns = false
    forwardable = true
    # Exercise all four AES enctypes flint claims to support.
    default_tgs_enctypes = aes256-cts-hmac-sha384-192 aes256-cts-hmac-sha1-96 aes128-cts-hmac-sha256-128 aes128-cts-hmac-sha1-96
    default_tkt_enctypes = aes256-cts-hmac-sha384-192 aes256-cts-hmac-sha1-96 aes128-cts-hmac-sha256-128 aes128-cts-hmac-sha1-96
    permitted_enctypes   = aes256-cts-hmac-sha384-192 aes256-cts-hmac-sha1-96 aes128-cts-hmac-sha256-128 aes128-cts-hmac-sha1-96

[realms]
    $REALM = {
        kdc = localhost
        admin_server = localhost
    }

[domain_realm]
    .flint.test = $REALM
    flint.test = $REALM
CONF

sudo tee /etc/krb5kdc/kdc.conf >/dev/null <<CONF
[kdcdefaults]
    kdc_ports = 88
    kdc_tcp_ports = 88

[realms]
    $REALM = {
        database_name = /var/lib/krb5kdc/principal
        admin_keytab = /etc/krb5kdc/kadm5.keytab
        acl_file = /etc/krb5kdc/kadm5.acl
        key_stash_file = /etc/krb5kdc/stash
        max_life = 10h 0m 0s
        max_renewable_life = 7d 0h 0m 0s
        supported_enctypes = aes256-cts-hmac-sha384-192:normal aes256-cts-hmac-sha1-96:normal aes128-cts-hmac-sha256-128:normal aes128-cts-hmac-sha1-96:normal
    }
CONF

sudo rm -f /var/lib/krb5kdc/principal* /etc/krb5kdc/stash
echo '*/admin *' | sudo tee /etc/krb5kdc/kadm5.acl >/dev/null
sudo kdb5_util create -s -r $REALM -P "$PW" 2>&1 | tail -2

# One service principal per enctype so every code path gets a real ticket.
for e in aes128-cts-hmac-sha1-96 aes256-cts-hmac-sha1-96 aes128-cts-hmac-sha256-128 aes256-cts-hmac-sha384-192; do
  short=$(echo $e | sed 's/-cts-hmac-sha/_/; s/-96$//; s/-192$//; s/-128$//' | tr -d '-')
  sudo kadmin.local -q "addprinc -randkey -e $e:normal nfs/$short.flint.test@$REALM" >/dev/null 2>&1
  sudo kadmin.local -q "ktadd -k /tmp/flint.keytab -e $e:normal nfs/$short.flint.test@$REALM" >/dev/null 2>&1
done
sudo kadmin.local -q "addprinc -pw $PW testuser@$REALM" >/dev/null 2>&1

sudo systemctl restart krb5-kdc
sleep 2
systemctl is-active krb5-kdc
sudo chmod 644 /tmp/flint.keytab
echo "--- keytab ---"
klist -k /tmp/flint.keytab
