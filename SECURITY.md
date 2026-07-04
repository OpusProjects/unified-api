# Security Policy

## Supported versions

Only the latest release line receives security fixes.

| Version | Supported |
|---|---|
| 0.x (latest release) | Yes |
| older tags | No |

## Reporting a vulnerability

Please **do not open a public issue** for security problems.

Use GitHub's private vulnerability reporting instead:
[Report a vulnerability](https://github.com/OpusProjects/unified-api/security/advisories/new)
— it opens a private thread with the maintainers.

Include what you can: affected endpoint or component, reproduction steps, and
impact. You should hear back within a week. Once a fix ships, the advisory is
published and credited unless you prefer otherwise.

## Scope notes

- Unified API is designed to run on a **trusted internal network**. The API key
  (`UNIFIED_API_KEY`) is a shared static secret; treat network exposure and key
  handling as part of your deployment's threat model.
- Connector, enricher and output scripts run **with the daemon's privileges**.
  Config files (and the scripts they point at) are trusted input — protect who
  can write them.
- Credentials are never stored by the service; they are read from environment
  variables or files at sync time and passed to scripts via their environment.
