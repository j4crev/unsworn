# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue: on this
repository's **Security** tab, choose **Report a vulnerability**. That opens
a private advisory only the maintainers can see.

A good report says what an attacker can do, which version or commit you
looked at, and the smallest config and request that show it.

You'll get an answer within a week. Once a fix is released, the advisory is
published, with credit to you unless you'd rather not be named.

## Supported versions

Fixes go into the latest release. Unsworn is pre-1.0, so older releases
don't get patches: upgrade to the newest one.

## What counts

Unsworn enforces a project's rules in front of its handlers, so the reports
that matter most are ways around them:

- reaching a route, a tool or a proxied path without the `auth`, `ip`,
  `origin` or `rate_limit` its config sets
- reading another caller's response from the cache
- a request that makes the server crash, hang or use memory without bound
- anything that runs code or reads files outside what the config allows

Handlers are the project's own code and are trusted. A handler reading
what its own process can read isn't a vulnerability in Unsworn, though
Unsworn does keep its own credentials out of their environment.

## Checking a release

Release archives and the container image carry a build attestation, signed
by GitHub, that names the workflow run and commit that built them:

```sh
gh attestation verify unsworn-x86_64-unknown-linux-musl.tar.gz --repo j4crev/unsworn
gh attestation verify oci://ghcr.io/j4crev/unsworn:latest --repo j4crev/unsworn
```

`install.sh` runs the first check when `UNSWORN_VERIFY=1` is set.
