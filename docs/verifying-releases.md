# Verify a release image

Any party can verify that the release workflow built an image.

1. Install cosign.
2. Run this command. Replace `vX.Y.Z` with the release tag and the digest
   with the digest you want to check.

```console
cosign verify \
  --certificate-identity-regexp '^https://github\.com/Yornik/Siemens-home-connect-exporter/\.github/workflows/release\.yml@refs/heads/main$' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  --certificate-github-workflow-repository 'Yornik/Siemens-home-connect-exporter' \
  -a "version=vX.Y.Z" \
  ghcr.io/yornik/siemens-home-connect-exporter@sha256:<digest>
```

3. cosign prints the verified signature and its annotations. An exit code of
   0 means that the release workflow built this image as version `vX.Y.Z`.

The release workflow runs the same verification before it publishes the
draft release. A release that exists passed this check.
