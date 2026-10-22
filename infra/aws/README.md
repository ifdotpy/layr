# Layr Cloud on AWS

These templates deploy a project store, a gateway, and an executor on Amazon Web Services (AWS).
The gateway and executor use the same `layr` release binary.
No command in this runbook requires Node.js.
The [Cloud design](../../docs/cloud-design.md) defines the contracts.

| File | Resources |
| --- | --- |
| `bootstrap.yaml` | Account artifacts bucket, retained, with versioning and encryption |
| `template.yaml` | Project bucket, retained, with reader, writer, and GC roles |
| `gateway.yaml` | IPv6-only network, Graviton gateway, instance role |
| `executor.yaml` | Graviton executor, instance role, retained gp3 data volume |
| `dns.yaml` | Optional Route 53 zone, retained |
| `sync.py` | Host setup, release install, gateway config, certificates |

The gateway has no direct access to project buckets.
It assumes the exact project roles that `StoreRoleArns` names.
The executor can assume the reader and writer roles, but cannot assume the GC role.
The host roles can also read their release artifacts and tailnet key.
The gateway can change its DNS records when `Host` and `ZoneId` are set.

Identity and Access Management (IAM) separates the project grants.
The garbage collection (GC) role handles remote data deletion.
The writer can create objects and abort multipart uploads.
It cannot read or delete objects.
The bucket requires `If-None-Match: *` for each object commit, including multipart completion.
Multipart setup and part uploads do not require that header.
The reader can read objects and list the project prefix.
The GC role can delete only `<prefix>/states/*/*.data.age`.
It cannot delete trees, records, slots, or object versions.
The application checks the 24-hour grace period and references before each GC operation.
The bucket aborts incomplete multipart uploads after seven days.
It retains complete objects and old object versions.
See the [AWS conditional-write policy reference](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes-enforce.html).

## Requirements

- AWS CLI v2 with a deployment profile and permission to create the template resources.
- `jq`, Python 3, `tar`, and a static Linux ARM64 `layr` binary.
- A Debian 13 ARM64 Amazon Machine Image (AMI) with cloud-init and IPv6 support.
- A tailnet with MagicDNS, HTTPS certificates, Tailscale SSH, and the tags below.
- A project with a store remote and a machine slot for each executor.

The templates use IPv6 for package downloads, AWS APIs, and Tailscale.
They do not assign public IPv4 addresses or create a NAT gateway.
The security groups accept only WireGuard UDP port 41641.
The gateway binds HTTPS to its tailnet IPv4 address.
The executor registry must also bind to its tailnet address.
Tailscale can relay connections from clients without IPv6.
See the [AWS IPv6-only subnet reference](https://aws.amazon.com/blogs/networking-and-content-delivery/introducing-ipv6-only-subnets-and-ec2-instances/).

Allow these tailnet connections:

- Members to `tag:layr-gateway` on TCP port 443.
- Members to `tag:layr-executor` on TCP port 5000, if they use the registry.
- `tag:layr-executor` to `tag:layr-gateway` on TCP port 443.
- Administrators to both tags through Tailscale SSH as root.

Assign tag ownership to administrators.
Use separate reusable, pre-approved auth keys for the two tags.
Do not grant either host access to other tailnet devices unless a task requires it.

## Set deployment values

Run all local commands from the repository root.
Replace the example values before use.
Use one AWS account and region for this example.
A project in another account can trust the gateway role ARN in `GatewayRoleArns`.

```sh
export AWS_PROFILE=layr-admin
export AWS_REGION=eu-central-1
export AWS_DEFAULT_REGION="$AWS_REGION"
export PROJECT=example
export GATEWAY_STACK=layr-gateway
export EXECUTOR_STACK=layr-executor-example
export GATEWAY_ROLE_NAME=layr-gateway
export EXECUTOR_ROLE_NAME=layr-executor-example
export ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
export PARTITION=$(aws sts get-caller-identity --query Arn --output text | cut -d: -f2)
export GATEWAY_ROLE_ARN="arn:$PARTITION:iam::$ACCOUNT_ID:role/$GATEWAY_ROLE_NAME"
export EXECUTOR_ROLE_ARN="arn:$PARTITION:iam::$ACCOUNT_ID:role/$EXECUTOR_ROLE_NAME"
export IMAGE_ID=ami-REPLACE_WITH_DEBIAN_13_ARM64
export LAYR_BINARY=/absolute/path/to/static-arm64/layr

output() {
  aws cloudformation describe-stacks --stack-name "$1" \
    --query "Stacks[0].Outputs[?OutputKey=='$2'].OutputValue | [0]" --output text
}
```

Use different executor stack and role names for each project.
A store prefix accepts letters, digits, underscores, and hyphens, up to 44 characters.
The gateway project name and the store prefix use the same value in this runbook.
Keep the role names stable because project trust policies name their ARNs.

Confirm the AMI before deployment:

```sh
aws ec2 describe-images --image-ids "$IMAGE_ID" \
  --query 'Images[0].{Name:Name,Owner:OwnerId,Architecture:Architecture,Root:RootDeviceName}'
```

Use an ARM64 image from a publisher that you trust.
The root device must be `/dev/xvda` for these templates.

## Create the artifacts bucket and store

```sh
aws cloudformation deploy --stack-name layr-bootstrap \
  --template-file infra/aws/bootstrap.yaml
export ARTIFACTS_BUCKET=$(output layr-bootstrap ArtifactsBucket)

aws cloudformation deploy --stack-name "layr-store-$PROJECT" \
  --template-file infra/aws/template.yaml --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides "Prefix=$PROJECT" \
    "GatewayRoleArns=$GATEWAY_ROLE_ARN,$EXECUTOR_ROLE_ARN"
export STORE_BUCKET=$(output "layr-store-$PROJECT" BucketName)
export STORE_URL=$(output "layr-store-$PROJECT" StoreUrl)
export READER_ROLE_ARN=$(output "layr-store-$PROJECT" ReaderRoleArn)
export WRITER_ROLE_ARN=$(output "layr-store-$PROJECT" WriterRoleArn)
export GC_ROLE_ARN=$(output "layr-store-$PROJECT" GcRoleArn)
```

The trust condition accepts future role ARNs.
Thus, the store can exist before either host role exists.
The templates create no IAM users or access keys.
Add a local AWS role ARN to `GatewayRoleArns` if that role must assume the project roles.
That role also needs its own `sts:AssumeRole` permission.
An administrator can initialize the store with direct bucket permissions.
The bucket still requires conditional creation from administrators.

Initialize the project remote from a trusted project machine:

```sh
(
  set +x
  eval "$(aws configure export-credentials --profile "$AWS_PROFILE" --format env)"
  layr --project "$PROJECT" remote add origin "$STORE_URL" --paper-key-out /secure/path/layr-paper-key
  layr --project "$PROJECT" push origin
)
```

Direct S3 requests use environment credentials.
The [AWS credential export command](https://docs.aws.amazon.com/cli/latest/reference/configure/export-credentials.html) supplies the profile credentials inside this subshell.
Keep the paper key outside AWS.
Never copy it to the executor, including its temporary folders.
Copy the recipient from `<prefix>/store.json` for the gateway config below:

```sh
aws s3api get-object --bucket "$STORE_BUCKET" --key "$PROJECT/store.json" \
  /tmp/layr-store.json >/dev/null
export STORE_RECIPIENT=$(jq -r .recipient /tmp/layr-store.json)
```

## Store the tailnet keys in SSM

AWS Systems Manager Parameter Store (SSM) stores the keys as `SecureString` values.
These commands use the default SSM encryption key.
A customer KMS key requires a separate, restricted `kms:Decrypt` grant on each host role.

Create two local files with mode `0600`.
Put only the respective tailnet key in each file.
Avoid a final newline.

```sh
aws ssm put-parameter --name /layr-store/gateway/tailscale-auth-key \
  --type SecureString --value file:///absolute/path/gateway-auth-key --overwrite
aws ssm put-parameter --name /layr-store/executor/tailscale-auth-key \
  --type SecureString --value file:///absolute/path/executor-auth-key --overwrite
```

Delete the local files after the SSM writes succeed.
Do not put keys in CloudFormation parameters, outputs, shell history, or artifacts.
Rotate the SSM values before a replacement host joins the tailnet.
An existing host keeps its Tailscale identity on its root disk.

## Publish a release

The archive contains only `./layr` and `./sync.py`.
The host checks the SHA-256 digest and rejects links, other files, and non-static ARM64 programs.
The artifacts bucket is the release authority, so restrict its write permissions to administrators.
The digest detects damage; it does not authenticate a malicious artifacts-bucket writer.

A native ARM64 Linux build can use these commands when the Rust toolchain and musl compiler exist:

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
export LAYR_BINARY="$PWD/target/aarch64-unknown-linux-musl/release/layr"
```

Create the archive locally:

```sh
export RELEASE_DIR=$(mktemp -d)
install -m 0755 "$LAYR_BINARY" "$RELEASE_DIR/layr"
install -m 0644 infra/aws/sync.py "$RELEASE_DIR/sync.py"
COPYFILE_DISABLE=1 tar -czf "$RELEASE_DIR/release.tgz" -C "$RELEASE_DIR" ./layr ./sync.py
export RELEASE_SHA=$(python3 -c \
  'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' \
  "$RELEASE_DIR/release.tgz")
for KIND in gateway executor; do
  aws s3 cp "$RELEASE_DIR/release.tgz" \
    "s3://$ARTIFACTS_BUCKET/$KIND/releases/$RELEASE_SHA.tgz"
  jq -n --arg key "$KIND/releases/$RELEASE_SHA.tgz" --arg sha256 "$RELEASE_SHA" \
    '{key:$key,sha256:$sha256}' > "$RELEASE_DIR/$KIND-release.json"
  aws s3 cp "$RELEASE_DIR/$KIND-release.json" \
    "s3://$ARTIFACTS_BUCKET/$KIND/release.json"
done
```

## Select the certificate and publish the gateway config

For a tailnet certificate, enable HTTPS certificates in the Tailscale administration console.
The host runs `tailscale cert` at first boot and every five minutes.
It uses the machine's MagicDNS name and renews the certificate when necessary.

```sh
export GATEWAY_HOST=''
export ZONE_ID=''
printf '%s\n' '{"tls":"tailscale"}' > "$RELEASE_DIR/gateway.json"
```

For a custom domain, deploy the DNS zone instead:

```sh
export DOMAIN=example.com
export GATEWAY_HOST=layr.example.com
aws cloudformation deploy --stack-name layr-dns \
  --template-file infra/aws/dns.yaml --parameter-overrides "Domain=$DOMAIN"
export ZONE_ID=$(output layr-dns ZoneId)
output layr-dns NameServers
jq -n --arg host "$GATEWAY_HOST" --arg zone "$ZONE_ID" \
  '{host:$host,zone:$zone,tls:"files"}' > "$RELEASE_DIR/gateway.json"
```

Set the registrar's nameservers to the zone output before first boot.
The host sets the domain's A record to its tailnet address.
Certbot uses the DNS challenge and requires no public HTTPS port.
The installed certbot timer renews the certificate.
Its deploy hook restarts the gateway.

Publish the host and project configs:

```sh
aws s3 cp "$RELEASE_DIR/gateway.json" "s3://$ARTIFACTS_BUCKET/gateway/gateway.json"
jq -n --arg bucket "$STORE_BUCKET" --arg region "$AWS_REGION" --arg prefix "$PROJECT" \
  --arg reader "$READER_ROLE_ARN" --arg writer "$WRITER_ROLE_ARN" --arg gc "$GC_ROLE_ARN" \
  --arg recipient "$STORE_RECIPIENT" \
  '{bucket:$bucket,region:$region,prefix:$prefix,readerRoleArn:$reader,writerRoleArn:$writer,
    gcRoleArn:$gc,recipient:$recipient,writeTokenHashes:[],gcTokenHashes:[]}' \
  > "$RELEASE_DIR/project.json"
aws s3 cp "$RELEASE_DIR/project.json" "s3://$ARTIFACTS_BUCKET/gateway/projects/$PROJECT.json"
```

The project files contain only token hashes, never bearer tokens or store identities.
The host writes `/etc/layr-gateway/config.json` with `host`, `listen`, and `projects`.
The listen value is the tailnet address plus port 443.
Systemd passes the certificate and key through `LoadCredential=tls.crt` and `LoadCredential=tls.key`.
The service runs as a dynamic user with a read-only system.
It starts `layr gateway --config /etc/layr-gateway/config.json`.

## Deploy the gateway

```sh
aws cloudformation deploy --stack-name "$GATEWAY_STACK" \
  --template-file infra/aws/gateway.yaml --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides "ArtifactsBucket=$ARTIFACTS_BUCKET" "ImageId=$IMAGE_ID" \
    "RoleName=$GATEWAY_ROLE_NAME" \
    "StoreRoleArns=$READER_ROLE_ARN,$WRITER_ROLE_ARN,$GC_ROLE_ARN" \
    "Host=$GATEWAY_HOST" "ZoneId=$ZONE_ID" \
    AuthKeyParameter=/layr-store/gateway/tailscale-auth-key
export GATEWAY_INSTANCE=$(output "$GATEWAY_STACK" InstanceId)
```

CloudFormation completion proves resource creation, but does not prove host setup.
Wait for `layr-gateway` to appear in the tailnet.
Check the first boot from Tailscale SSH:

```sh
tailscale ssh root@layr-gateway \
  'cloud-init status --wait; systemctl status layr-gateway-sync.service layr-gateway.service --no-pager'
tailscale ssh root@layr-gateway 'journalctl -u layr-gateway-sync.service -n 50 --no-pager'
tailscale ssh root@layr-gateway 'cat /etc/layr-gateway/config.json'
```

For a tailnet certificate, use the `host` from the generated config as `GATEWAY_HOST`.
Check `https://<host>/v1/p/<project>/store.json` from a tailnet client.
Use an authenticated client to check a challenge, fetch, and conditional push.

The release timer retries setup after a transient release, SSM, DNS, or certificate failure.
A package-install failure appears in `/var/log/cloud-init-output.log`.
Correct the package or network fault before you rerun that failed first-boot step.

## Issue write and GC tokens

Issue tokens on the gateway host:

```sh
tailscale ssh root@layr-gateway \
  "/opt/layr-gateway/current/layr gateway token $PROJECT --write --until 24h"
tailscale ssh root@layr-gateway \
  "/opt/layr-gateway/current/layr gateway token $PROJECT --gc --until 1h"
```

Store each output token in the client's credential store.
Treat a GC token as a separate grant.
The gateway keeps only SHA-256 token hashes in its config.
Unchanged remote config does not replace tokens that the host issued locally.
Before any project-config change, copy the current project entry into the artifacts bucket:

```sh
tailscale ssh root@layr-gateway 'cat /etc/layr-gateway/config.json' \
  | jq --arg project "$PROJECT" '.projects[$project]' > "$RELEASE_DIR/project.json"
aws s3 cp "$RELEASE_DIR/project.json" "s3://$ARTIFACTS_BUCKET/gateway/projects/$PROJECT.json"
```

A remote config change replaces the local project map, including token hashes.
Back up every project's current hashes before you change any gateway project or host config.
To revoke a token, remove its hash from the project artifact and the local config.
Do not remove a store slot to revoke a write-only token.

## Deploy the executor

The executor uses the gateway subnet and its IPv6 route.
The separate gp3 volume remains after stack deletion or volume replacement.
First boot identifies the volume by its Elastic Block Store (EBS) serial number.
It formats only an empty disk and refuses any other filesystem or partition table.
The `layr-data.service` unit mounts btrfs at `/var/lib/layr` after each boot.
The Layr service cannot start when that mount fails.

```sh
export SUBNET_ID=$(output "$GATEWAY_STACK" SubnetId)
export VPC_ID=$(output "$GATEWAY_STACK" VpcId)
export AVAILABILITY_ZONE=$(aws ec2 describe-subnets --subnet-ids "$SUBNET_ID" \
  --query 'Subnets[0].AvailabilityZone' --output text)
aws cloudformation deploy --stack-name "$EXECUTOR_STACK" \
  --template-file infra/aws/executor.yaml --capabilities CAPABILITY_NAMED_IAM \
  --parameter-overrides "ArtifactsBucket=$ARTIFACTS_BUCKET" "ImageId=$IMAGE_ID" \
    "VpcId=$VPC_ID" "SubnetId=$SUBNET_ID" "AvailabilityZone=$AVAILABILITY_ZONE" \
    "RoleName=$EXECUTOR_ROLE_NAME" "TailnetHostname=$EXECUTOR_ROLE_NAME" \
    "ReaderRoleArn=$READER_ROLE_ARN" \
    "WriterRoleArn=$WRITER_ROLE_ARN" InstanceType=t4g.small DataVolumeSize=100 \
    AuthKeyParameter=/layr-store/executor/tailscale-auth-key
export EXECUTOR_INSTANCE=$(output "$EXECUTOR_STACK" InstanceId)
export DATA_VOLUME_ID=$(output "$EXECUTOR_STACK" DataVolumeId)
tailscale ssh root@"$EXECUTOR_ROLE_NAME" \
  'cloud-init status --wait; systemctl status layr-data.service layr-executor-sync.service --no-pager'
```

The Layr service and transfer operations wait until `/etc/layr-executor/ready` exists.
First boot enables the transfer timer.
Use the following profile to add the executor machine slot.

## Configure the executor profile

Issue a write token for the executor from the local administration machine.
Store it in a file with mode `0600`:

```sh
umask 077
tailscale ssh root@layr-gateway \
  "/opt/layr-gateway/current/layr gateway token $PROJECT --write --until 720h" \
  > "$RELEASE_DIR/executor-token"
tailscale ssh root@"$EXECUTOR_ROLE_NAME" \
  'install -m 0600 /dev/null /etc/layr-executor/write-token; cat > /etc/layr-executor/write-token' \
  < "$RELEASE_DIR/executor-token"
rm "$RELEASE_DIR/executor-token"
```

First boot runs `layr remote self --json` after the data mount.
It writes `/etc/layr-executor/machine.json` and prints its public fields to the service journal.
It also prints those fields to the first-boot console log.
Read the executor's public identity from the local administration machine:

```sh
tailscale ssh root@"$EXECUTOR_ROLE_NAME" 'cat /etc/layr-executor/machine.json' \
  > "$RELEASE_DIR/executor-machine.json"
```

On the trusted Mac, use the machine ID and Ed25519 public key to create the executor's slot:

```sh
export EXECUTOR_MACHINE_ID=$(jq -r .id "$RELEASE_DIR/executor-machine.json")
export EXECUTOR_PUBLIC_KEY=$(jq -r .public_key "$RELEASE_DIR/executor-machine.json")
(
  set +x
  eval "$(aws configure export-credentials --profile "$AWS_PROFILE" --format env)"
  layr --project "$PROJECT" remote trust origin "$EXECUTOR_MACHINE_ID" --public-key "$EXECUTOR_PUBLIC_KEY"
)
```

The trusted Mac opens its own slot or paper key locally.
Only the encrypted machine slot reaches the store.
The executor never receives the paper key or another machine's private key.

Run these commands in a root Bash shell on the executor:

```sh
set -euo pipefail
export LAYR_ROOT=/var/lib/layr
export PROJECT=example
export GATEWAY_HOST=layr-gateway.YOUR-TAILNET.ts.net
export LAYR_GATEWAY_TOKEN_FILE=/etc/layr-executor/write-token
ln -sfn /opt/layr-executor/current/layr /usr/local/bin/layr
layr clone "layr+https://$GATEWAY_HOST/$PROJECT" "$PROJECT"
layr --project "$PROJECT" fetch origin
layr --project "$PROJECT" config queue.run main
layr --project "$PROJECT" config registry.listen "$(tailscale ip -4):5000"
layr --project "$PROJECT" config registry.user layr-registry
printf 'PROJECT=%q\n' "$PROJECT" > /etc/layr-executor/profile.env
chmod 0600 /etc/layr-executor/profile.env
touch /etc/layr-executor/ready
systemctl enable --now layr.service layr-transfer.timer
systemctl start layr-transfer.service
systemctl status layr.service layr-transfer.service --no-pager
findmnt -no SOURCE,FSTYPE,TARGET /var/lib/layr
```

Clone creates the local project with the remote project identity and configures `origin`.
Clone and fetch use the machine slot that the trusted Mac created.

For an existing copy of the same project without `origin`, add the gateway remote directly:

```sh
layr --project "$PROJECT" remote add origin "layr+https://$GATEWAY_HOST/$PROJECT"
```

The units set `LAYR_GATEWAY_TOKEN_FILE=/etc/layr-executor/write-token`.
The client refuses token files with another owner or a mode other than `0600`.
Read requests use the machine slot and challenge; they need no stored bearer token.
Replace the token file before the token expires.
Keep bearer tokens out of command arguments, project config, records, and logs.

The root daemon runs the queue and project checks.
First boot creates the dedicated system account `layr-registry` without a home or login shell.
The registry handler uses that unprivileged account.
Do not use that account for ordinary Layr commands.
The timer fetches and pushes `origin` after each inactive minute.
Systemd permits only one transfer unit at a time.
A fetch failure stops the push and appears in the unit journal.
The executor never runs remote GC automatically.
Restart `layr.service` after any registry listener or user change.

## Upgrade and recover

Publish a new archive and both release manifests with the release commands above.
The host checks its manifest every five minutes.
It switches the release symlink only after the digest and archive checks pass.
It retains the current release and one previous release.
It restarts the gateway or Layr service when the binary changes.

Force a check from Tailscale SSH when necessary:

```sh
tailscale ssh root@layr-gateway 'systemctl start layr-gateway-sync.service'
tailscale ssh root@"$EXECUTOR_ROLE_NAME" 'systemctl start layr-executor-sync.service'
```

For rollback, publish the previous archive's key and SHA-256 in each release manifest.
Do not change only the host symlink because the next timer restores the manifest release.
Artifacts bucket versioning retains previous manifests.

Use `aws cloudformation deploy` with the same template and full parameter list for infrastructure updates.
User data runs only on first boot.
A changed user-data value does not rerun host setup on an existing root disk.
Do not replace an executor instance without a plan to preserve its data volume and machine identity.
Stop the service and transfer timer before you detach the data volume.
A retained volume needs explicit reattachment or resource import; a new stack creates a new volume.

## Teardown

Save the bucket names and volume ID before deletion:

```sh
printf 'store=%s\nartifacts=%s\ndata=%s\n' "$STORE_BUCKET" "$ARTIFACTS_BUCKET" "$DATA_VOLUME_ID"
tailscale ssh root@"$EXECUTOR_ROLE_NAME" 'systemctl stop layr-transfer.timer layr-transfer.service layr.service'
aws cloudformation delete-stack --stack-name "$EXECUTOR_STACK"
aws cloudformation wait stack-delete-complete --stack-name "$EXECUTOR_STACK"
aws cloudformation delete-stack --stack-name "$GATEWAY_STACK"
aws cloudformation wait stack-delete-complete --stack-name "$GATEWAY_STACK"
aws cloudformation delete-stack --stack-name "layr-store-$PROJECT"
aws cloudformation wait stack-delete-complete --stack-name "layr-store-$PROJECT"
aws cloudformation delete-stack --stack-name layr-bootstrap
aws cloudformation wait stack-delete-complete --stack-name layr-bootstrap
aws ssm delete-parameters --names /layr-store/gateway/tailscale-auth-key \
  /layr-store/executor/tailscale-auth-key
```

Delete the executor first because it uses the gateway subnet.
The project bucket, artifacts bucket, and data volume remain, with their storage charges.
Do not delete the shared gateway, artifacts, or SSM keys while another project uses them.
Remove the two host devices from the tailnet administration console.
Revoke the two tailnet auth keys.

For a custom domain, delete its records and update registrar delegation before you remove the zone.
The DNS stack retains the zone:

```sh
aws cloudformation delete-stack --stack-name layr-dns
aws cloudformation wait stack-delete-complete --stack-name layr-dns
# After you remove all records except the default SOA and NS records:
aws route53 delete-hosted-zone --id "$ZONE_ID"
```

To remove a retained data volume permanently, first confirm that no recovery data remains:

```sh
aws ec2 delete-volume --volume-id "$DATA_VOLUME_ID"
```

A versioned bucket needs all object versions and delete markers removed before `delete-bucket` succeeds.
Use the S3 console's **Empty** action on the exact retained bucket after you verify the recovery copy.
Then use these commands for permanent bucket deletion:

```sh
aws s3api delete-bucket --bucket "$STORE_BUCKET"
aws s3api delete-bucket --bucket "$ARTIFACTS_BUCKET"
```

## Local checks

Run the checks in the OrbStack VM. They create no AWS resources.

```sh
orb -m layr-dev -u root bash -lc \
  'python3 -m venv /opt/layr-cfn-lint && /opt/layr-cfn-lint/bin/pip install cfn-lint boto3'
orb -m layr-dev -u root bash -lc \
  '/opt/layr-cfn-lint/bin/cfn-lint /path/to/layr/infra/aws/*.yaml'
orb -m layr-dev -u root bash -lc \
  '/opt/layr-cfn-lint/bin/python -m unittest discover -s /path/to/layr/infra/aws -p "test_*.py" -v'
```

Install `python3-venv` in the VM first if its Python installation lacks `ensurepip`.
The volume tests create temporary loop devices and test btrfs mounts as root.
They reject ext4 and preserve data across repeat mounts.
The release tests use synthetic ELF headers. They do not execute the `layr` binary.
A template lint result does not prove AWS deployment, first boot, or a live gateway request.
