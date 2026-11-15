# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: on the repository's **Security** tab, choose **Report a vulnerability** (a private security advisory). Do not open a public issue, and do not include a real token or credentials in the report.

You can expect an acknowledgement within a week. Fixes are released as a new version and described in the advisory and the changelog once users can update.

## Supported versions

nit is pre-1.0: only the latest release receives security fixes.

## Scope

In scope, among others:

- Token handling: the Atlassian email and API token are stored only in the OS credential store, service `nit` (Windows Credential Manager, the macOS Keychain, the Secret Service on Linux; never a file fallback). They must never reach the cache (`cache.sqlite` in the data folder, on Windows `%LOCALAPPDATA%\nit`), settings, logs, `crash.log`, `token-check.txt`, error messages, git config, remote URLs or command lines.
- Git credentials: git receives the token through environment variables of the git process only, for that one command.
- Network: requests carrying credentials go only to the configured Bitbucket origin; redirects to other origins are not followed.
- What is never logged: tokens, passwords, `Authorization` headers, request URLs with credentials and response bodies. User-facing errors show a category (or Bitbucket's own short message for a refused write) instead.
- Anything that makes nit post, approve or change something on Bitbucket without the user's confirmation.

Out of scope: the security of Bitbucket itself, of Git for Windows, and of a machine an attacker already controls (for example someone who can read your Windows Credential Manager).
