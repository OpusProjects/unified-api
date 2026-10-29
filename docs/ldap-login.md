# LDAP login

The browsable UI's login form can take a directory account (LDAP or Active
Directory) as well as an API key. People log in with the username and password
they already have; what they may touch is decided by **access rules in this
service's configuration**, in the same vocabulary the `api_keys` items use.

The directory says who someone is and which groups they belong to. It never
decides access by itself: a new group in AD grants nothing until a rule names
it.

## Configuration

`config.yaml`, under `server.ui` (LDAP login is part of the browsable UI, so
`enabled: true` is required):

```yaml
server:
  ui:
    enabled: true
    home: "/"                               # where a sign-in lands; see "Sessions"
    ldap:
      url: "ldaps://ldap.example.com:636"   # or ldap://...:389 with starttls: true
      bind_credential: cred-ldap            # a credentials item: username = bind DN, password
      search_base: "OU=Users,DC=example,DC=com"
      # user_filter: "(sAMAccountName={username})"   # the default (Active Directory)
      # group_attribute: memberOf                     # the default
      # ca_file: /etc/ssl/private-ca.pem              # a private CA; default = system store
      # timeout_seconds: 10
      access:
        - name: "Operations"
          groups: [DL-pcol2operations]      # a group CN, or a full DN
          role: admin
        - name: "VMware readers"
          groups: ["CN=DL-vmware,OU=Groups,DC=example,DC=com"]
          users: [jane.doe]                 # people without a group, by username
          role: restricted                  # the default
          sources: [src-vmware]
          endpoints: [ep-ansible-vmware]
```

The service account is a `credentials` item like any source's credential, so it
can live in env vars, a file or Vault:

```yaml
# credentials/cred-ldap.yaml
credentials:
  - name: cred-ldap
    description: "LDAP service account"
    type: username_password
    vault: cred-vault                 # a `type: vault` credential
    vault_path: unified-api/ldap      # or env_vars / secret_file instead
```

Its `username` is the **bind DN** (`CN=svc-unified,OU=Service,DC=example,DC=com`)
and `password` its password. It only needs to read users and their group
attribute.

### Access rules

- Evaluated top to bottom, **all matches combined**: `admin` in any matching rule
  is admin; otherwise the `sources` and `endpoints` of every matching rule are
  unioned.
- A group written as a **CN** (`DL-ops`) matches any group with that CN; a **DN**
  matches only that group. Both case-insensitive.
- `role` defaults to `restricted`, the same safe default as API keys: a rule that
  forgets it never hands out admin.
- **Matching no rule refuses the login**, even with the right password. The person
  is told their account has no access, which leaks nothing (they proved the
  password) and saves a ticket.

### Validation

`--check-config` and the config API refuse, before anyone tries to log in:

- `ldap://` without `starttls: true`, since every password would cross the
  network in clear text;
- a `bind_credential` that names no `credentials` item;
- a `user_filter` without `{username}`;
- an empty `access` list, or a rule naming no groups and no users;
- a rule naming a source or endpoint that does not exist;
- `ldap:` with `server.ui.enabled: false`.

## How a login works

```
  form: username + password
    │
    ├─ bind as the service account        (bind_credential, via the secrets chain)
    ├─ search search_base for user_filter  ({username} escaped, RFC 4515)
    │     exactly one entry, or the login fails
    ├─ bind AS that entry with the password typed   (the directory checks it)
    ├─ read group_attribute (memberOf)
    └─ access rules → admin | restricted(sources, endpoints) | refused
```

One connection per login, closed straight after. No password or hash is ever
stored or readable here.

Guards worth knowing about:

- **An empty password is refused before the directory is contacted.** To LDAP, a
  DN with no password is an *unauthenticated bind*, which Active Directory and
  many other servers answer with success. Without this guard an empty password
  field would log anyone in as anyone.
- **The username is escaped** into the filter, so `*` or `)(` cannot rewrite it.
- **A filter matching more than one entry is refused** rather than picking the
  first, and logged.
- **Wrong password and unknown user get the same answer**, after a fixed pause.
  The directory's own lockout policy protects the accounts.
- **An unreachable directory is not a wrong password.** It answers `503`, logs
  the reason, and suggests the API key, which keeps working.

## Account names, limits, and the API keys it needs

- **Whose rule applies.** `access[].users` and the session's name use the
  account's own name, read from `username_attribute` (default
  `sAMAccountName`), not what was typed. With a filter that matches on several
  attributes, an e-mail address say, typing someone else's name cannot reach
  their rule.
- **Limits.** After 5 wrong passwords in 15 minutes an account is refused (429)
  without the directory being asked, so a script cannot lock the real account
  out through this form; at most 4 directory logins run at once; and every
  failed login answers after the same 1.5 seconds, so timing does not tell an
  unknown user from a wrong password.
- **At least one API key.** With no API key configured the API runs open: every
  caller is admin and no session is consulted. LDAP in that state would protect
  nothing, so it refuses to start, and a reload that would arrive at it is
  refused. An admin key is also the way in when the directory is down.

## Sessions

An LDAP login opens the same kind of browser session an API key does (in
memory, `HttpOnly`, `SameSite=Strict`, CSRF on every write). It is named
`ldap:<username>` in the access log.

After signing in, the browser goes back to the page that sent it there
(`?next=`, only a path on this server). With no such page (someone opening
`/api/v1/login` directly) it goes to `server.ui.home`: `/api/v1/` by default,
the API's own pages, and `"/"` where the UI is served in front of the API.
Signing out from the UI posts `next=/`, so signing straight back in returns to
the UI too.

What ends it:

- logout, or `session_ttl_seconds`;
- a restart;
- **a reload that changes `server.ui.ldap`**. A session carries the permissions
  its rules granted at login, so changed rules end the LDAP sessions and the next
  login is judged by the new ones. API key sessions are untouched (a key's
  removal revokes those), and a reload that changes only sources leaves LDAP
  sessions alone.

`server.ui.ldap` and `server.ui.home` reload live (`applied` in the reload
report). `server.ui.enabled` and `server.ui.session_ttl_seconds` need a restart:
a reload that changes them reports them in `restart_required`.

## Who is logged in: `GET /api/v1/auth`

Read-only, for the UI's Users page. Every authenticated caller gets `me` (name,
method `ldap` / `api_key` / `open`, role, scope). An admin also gets the LDAP
configuration with its access rules, and the API keys by name and scope. A
secret never appears. Users and the directory are declared in git and pushed
like the rest of the configuration, so this route describes what is in force
rather than offering a second place to change it.

## What it does not do

- **No local users.** People log in through the directory; machines and
  break-glass access use API keys.
- **No nested groups** yet: only the groups listed directly in `memberOf`. Active
  Directory's transitive match (`1.2.840.113556.1.4.1941`) can be added as an
  option if a rule needs it.
- **No editing from the UI.** Access is changed by a pull request to the
  configuration repository.
