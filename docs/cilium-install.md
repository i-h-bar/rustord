# Installing Cilium on the k3s Node

This replaces k3s's default Flannel CNI with Cilium, so that the
`CiliumNetworkPolicy` resources in `charts/rustcord/templates/networkpolicies.yaml`
are actually enforced. This is a one-time, manual, cluster-level operation —
not something the app's Helm release manages.

Cilium does not have first-class CNI-chaining support for Flannel, so this
is a full CNI replacement, not an addition. Pods lose networking for a
short window while the swap happens; on this single-node box that's an
acceptable brief outage, not a rolling migration.

## 1. Add the Cilium Helm repo (one-time, on the machine running `helm`)

```bash
helm repo add cilium https://helm.cilium.io/
helm repo update
```

## 2. Disable Flannel and k3s's built-in network policy controller

Edit `/etc/rancher/k3s/config.yaml` on the VM (create it if it doesn't
exist) and add:

```yaml
flannel-backend: "none"
disable-network-policy: true
```

Restart k3s:

```bash
sudo systemctl restart k3s
```

At this point existing pods will show networking errors — this is expected
until Cilium's agent DaemonSet comes up in the next step.

## 3. Find the node's IP address and update `cilium/values.yaml`

```bash
hostname -I | awk '{print $1}'
```

Replace `<NODE_IP>` in `cilium/values.yaml` with that address.

## 4. Install Cilium

From the dev machine (or the VM, wherever `helm` + a working `kubeconfig`
pointing at this cluster are available):

```bash
helm install cilium cilium/cilium --version 1.20.1 -n kube-system -f cilium/values.yaml
```

## 5. Verify

Install the Cilium CLI if not already present, then:

```bash
cilium status --wait
cilium connectivity test
```

Both should report healthy. `cilium connectivity test` creates temporary
test pods and confirms basic pod-to-pod and pod-to-world connectivity works
before any `CiliumNetworkPolicy` is applied.

## 6. Observability while testing policies

```bash
cilium hubble port-forward &
hubble observe --follow
```

Leave this running while exercising the bot (see the Testing Plan in
`docs/superpowers/specs/2026-08-28-cilium-network-policy-design.md`) —
allowed traffic shows as `FORWARDED`, blocked traffic as `DROPPED`.

## Rollback

If Cilium needs to be removed and Flannel restored:

```bash
helm uninstall cilium -n kube-system
```

Then revert the `flannel-backend`/`disable-network-policy` edits in
`/etc/rancher/k3s/config.yaml` and `sudo systemctl restart k3s`.
