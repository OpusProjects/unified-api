# Deployment manifests

Real, CI-linted starting points for the two deployment styles described in
[docs/deployment.md](../docs/deployment.md) — extracted from that page so you
apply files instead of copy-pasting from prose, and so CI can validate them
(`docker compose config -q` and kubeconform run on every PR).

- `compose/compose.yaml` — the single-host docker compose deployment. Put your
  `config/` directory, `.env` and `secrets/` next to it and `docker compose up -d`.
- `k8s/` — a complete Deployment (probes, non-root, resources), Service and
  PVC; `kubectl apply -k deploy/k8s/`. You supply the named ConfigMap and
  Secrets:

```bash
kubectl create configmap unified-api-config --from-file=config/
kubectl create secret generic unified-api-env --from-env-file=.env
kubectl create secret generic unified-api-secrets \
  --from-file=ssh-private-key=secrets/id_ed25519 \
  --from-file=gitlab.json=secrets/gitlab.json
```

Pin the image tag to the release you deploy (see the CHANGELOG for what each
version changes); `latest` always means the newest release, never the tip of
main. For GitOps (ArgoCD), secret variants (Sealed Secrets, ESO) and
multi-datacenter topologies, start from
[docs/deployment.md](../docs/deployment.md).
