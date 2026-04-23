# Vendored JSON Schemas

This directory holds JSON Schema files committed to the repo so CI
validation is offline and reproducible (independent of schemastore.org
availability and URL stability).

## `dependabot-2.0.json`

- **Source**: https://json.schemastore.org/dependabot-2.0.json
  (redirects to https://www.schemastore.org/dependabot-2.0.json)
- **Fetched**: 2026-04-23
- **SHA-256**: `1f61eb228202e5f7a394ce295eebf602b8b676dff0d51dbe489a9a9434263c51`
- **Used by**: [`scripts/dependabot-scripts-test.sh`](../../scripts/dependabot-scripts-test.sh)
  via `check-jsonschema --schemafile`.

To refresh:

```bash
curl -sSL -o .github/schemas/dependabot-2.0.json \
  https://json.schemastore.org/dependabot-2.0.json
shasum -a 256 .github/schemas/dependabot-2.0.json
# update the SHA-256 + Fetched lines above
bash scripts/dependabot-scripts-test.sh
```

Schemastore occasionally rewrites paths. If the URL stops resolving,
fall back to [SchemaStore's repo] and pick the latest
`dependabot-2.0.json`.

[SchemaStore's repo]: https://github.com/SchemaStore/schemastore
