"""Check the IAM boundaries and the shell/Python programs embedded in the templates."""

import re
import subprocess
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).parent


class Loader(yaml.SafeLoader):
    pass


def intrinsic(loader, tag, node):
    if isinstance(node, yaml.ScalarNode):
        value = loader.construct_scalar(node)
    elif isinstance(node, yaml.SequenceNode):
        value = loader.construct_sequence(node)
    else:
        value = loader.construct_mapping(node)
    return {tag if tag == "Ref" else f"Fn::{tag}": value}


Loader.add_multi_constructor("!", intrinsic)


def template(name):
    return yaml.load((ROOT / name).read_text(), Loader=Loader)


def statements(resource):
    result = []
    for policy in resource["Properties"]["Policies"]:
        result.extend(policy["PolicyDocument"]["Statement"])
    return result


def actions(resource):
    result = set()
    for statement in statements(resource):
        value = statement["Action"]
        result.update(value if isinstance(value, list) else [value])
    return result


class TemplateTests(unittest.TestCase):
    def test_project_iam(self):
        resources = template("template.yaml")["Resources"]
        self.assertEqual(actions(resources["Writer"]), {"s3:PutObject", "s3:AbortMultipartUpload", "s3:ListBucket"})
        writer_resources = statements(resources["Writer"])[0]["Resource"]
        self.assertEqual(writer_resources, [
            {"Fn::Sub": "${Bucket.Arn}/${Prefix}/records/*"},
            {"Fn::Sub": "${Bucket.Arn}/${Prefix}/states/*"},
            {"Fn::Sub": "${Bucket.Arn}/${Prefix}/slots/*"},
        ])
        self.assertNotIn({"Fn::Sub": "${Bucket.Arn}/${Prefix}/store.json"}, writer_resources)
        writer_list = statements(resources["Writer"])[1]
        self.assertEqual(writer_list["Action"], "s3:ListBucket")
        self.assertEqual(writer_list["Resource"], {"Fn::GetAtt": "Bucket.Arn"})
        self.assertEqual(writer_list["Condition"], {"StringLike": {"s3:prefix": [
            {"Fn::Sub": "${Prefix}/records/*"}, {"Fn::Sub": "${Prefix}/states/*"},
        ]}})
        self.assertEqual(actions(resources["Reader"]), {"s3:GetObject", "s3:ListBucket"})
        self.assertEqual(actions(resources["Gc"]), {"s3:DeleteObject"})
        self.assertEqual(statements(resources["Gc"])[0]["Resource"],
                         {"Fn::Sub": "${Bucket.Arn}/${Prefix}/states/*/*.data.age"})
        for role in ("Writer", "Reader", "Gc"):
            trust = resources[role]["Properties"]["AssumeRolePolicyDocument"]["Statement"][0]
            self.assertEqual(trust["Condition"]["ArnEquals"]["aws:PrincipalArn"], {"Ref": "GatewayRoleArns"})
        policy = resources["BucketPolicy"]["Properties"]["PolicyDocument"]["Statement"]
        conditional = next(item for item in policy if item["Sid"] == "RequireConditionalCreate")
        self.assertEqual(conditional["Principal"], "*")
        self.assertEqual(conditional["Condition"]["Null"]["s3:if-none-match"], "true")
        self.assertEqual(conditional["Condition"]["Bool"]["s3:ObjectCreationOperation"], "true")
        value = next(item for item in policy if item["Sid"] == "RequireCreateOnlyValue")
        self.assertEqual(value["Condition"]["StringNotEquals"]["s3:if-none-match"], "*")
        self.assertEqual(resources["Bucket"]["DeletionPolicy"], "Retain")

    def test_gc_removes_noncurrent_bytes_without_expiring_current_objects(self):
        bucket = template("template.yaml")["Resources"]["Bucket"]["Properties"]
        self.assertEqual(bucket["VersioningConfiguration"]["Status"], "Enabled")
        rules = bucket["LifecycleConfiguration"]["Rules"]
        rule = next(item for item in rules if item["Id"] == "RemoveGarbageCollectedStateData")
        self.assertEqual(rule["Status"], "Enabled")
        self.assertEqual(rule["Prefix"], {"Fn::Sub": "${Prefix}/states/"})
        self.assertEqual(rule["NoncurrentVersionExpiration"], {"NoncurrentDays": 30})
        self.assertTrue(rule["ExpiredObjectDeleteMarker"])
        self.assertNotIn("ExpirationInDays", rule)
        self.assertNotIn("ExpirationDate", rule)
        self.assertNotIn("NoncurrentVersionExpiration", rules[0])

    def test_host_roles_do_not_access_project_objects(self):
        for name, kind in (("gateway.yaml", "gateway"), ("executor.yaml", "executor")):
            role = template(name)["Resources"]["Role"]
            for item in statements(role):
                # Optional Route 53 statements use Fn::If.
                if "Action" not in item:
                    continue
                value = item["Action"]
                values = value if isinstance(value, list) else [value]
                if any(action.startswith("s3:") for action in values):
                    self.assertIn("${ArtifactsBucket}", item["Resource"]["Fn::Sub"])
                if "sts:AssumeRole" in values:
                    expected = {"Ref": "StoreRoleArns"} if kind == "gateway" else [
                        {"Ref": "ReaderRoleArn"}, {"Ref": "WriterRoleArn"}]
                    self.assertEqual(item["Resource"], expected)

    def test_host_security_and_retained_data(self):
        for name in ("gateway.yaml", "executor.yaml"):
            instance = template(name)["Resources"]["Instance"]["Properties"]
            self.assertEqual(instance["MetadataOptions"]["HttpTokens"], "required")
            self.assertEqual(instance["MetadataOptions"]["HttpProtocolIpv6"], "enabled")
            self.assertFalse(instance["NetworkInterfaces"][0]["AssociatePublicIpAddress"])
        executor = template("executor.yaml")["Resources"]["Instance"]["Properties"]["UserData"]["Fn::Base64"]["Fn::Sub"]
        self.assertIn("useradd --system --user-group --no-create-home --home-dir /nonexistent", executor)
        self.assertIn("--shell /usr/sbin/nologin layr-registry", executor)
        self.assertIn("Environment=LAYR_GATEWAY_TOKEN_FILE=/etc/layr-executor/write-token", executor)
        volume = template("executor.yaml")["Resources"]["DataVolume"]
        self.assertEqual(volume["Properties"]["VolumeType"], "gp3")
        self.assertTrue(volume["Properties"]["Encrypted"])
        self.assertEqual(volume["DeletionPolicy"], "Retain")
        self.assertEqual(volume["UpdateReplacePolicy"], "Retain")

    def test_user_data_programs(self):
        for name in ("gateway.yaml", "executor.yaml"):
            code = template(name)["Resources"]["Instance"]["Properties"]["UserData"]["Fn::Base64"]["Fn::Sub"]
            # CloudFormation values cannot affect shell grammar through this substitution.
            code = re.sub(r"\$\{[A-Za-z0-9:.]+\}", "example", code)
            result = subprocess.run(["bash", "-n"], input=code, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            for path, content in re.findall(r"cat > (\S+) <<'EOF'\n(.*?)\nEOF", code, re.S):
                if path.endswith(".py"):
                    compile(content, path, "exec")
                if path.startswith("/usr/local/sbin/"):
                    result = subprocess.run(["bash", "-n"], input=content, text=True, capture_output=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("nodejs", code)

    def test_runbook_shell_and_executor_secrets(self):
        runbook = (ROOT / "README.md").read_text()
        for code in re.findall(r"```sh\n(.*?)\n```", runbook, re.S):
            result = subprocess.run(["bash", "-n"], input=code, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
        executor_profile = runbook.split("## Configure the executor profile", 1)[1].split("## Upgrade", 1)[0]
        self.assertNotIn("LAYR_PAPER_KEY_FILE", executor_profile)
        self.assertNotIn("/run/layr-paper", executor_profile)
        self.assertIn("--public-key", executor_profile)
        self.assertIn("config registry.user layr-registry", executor_profile)


if __name__ == "__main__":
    unittest.main()
