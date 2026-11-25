# Security policy

There are two kinds of security problem you might find. They go to
different places.

- **This project**: the exporter code, its CI, or the container image.
  Report it to this repository, privately. See
  [Reporting a vulnerability in this project](#reporting-a-vulnerability-in-this-project).
- **Home Connect or the appliances**: the Home Connect cloud service, the
  app, or a BSH appliance. Report it to BSH, not to this repository. See
  [Vulnerabilities in Home Connect or the appliances](#vulnerabilities-in-home-connect-or-the-appliances).

This is an independent, unofficial hobby project. It is not made,
endorsed, or supported by BSH Hausgeräte GmbH or Robert Bosch GmbH. It
cannot fix, and does not speak for, anything on the BSH side.

## Reporting a vulnerability in this project

**Do not open a public issue.** Report it privately through GitHub:
**Security → Report a vulnerability** on this repository
([direct link](https://github.com/Yornik/Siemens-home-connect-exporter/security/advisories/new)).

Please include:

- what is affected: file, version, or image tag;
- how to reproduce it, without any real credential;
- what an attacker could do with it.

**Never include real credentials or tokens**, not even your own. Redact
them as `<redacted>`.

This is maintained in spare time, so handling is best effort:

- **Acknowledgement:** within 7 days.
- **Fix:** shipped as a new release, as soon as one is ready.
- **Disclosure:** coordinated with you, and a GitHub security advisory
  credits you unless you would rather not be named.

## Vulnerabilities in Home Connect or the appliances

BSH Hausgeräte GmbH runs the Home Connect service. It takes security
reports through the Bosch Product Security Incident Response Team (PSIRT).
Their channels, as BSH publishes them:

- E-mail: `psirt@bosch.com`. Write in English or German. They ask you to
  encrypt mail with their PGP key.
- Report page: <https://psirt.bosch.com/report-a-vulnerability/>
- Home Connect reporting page:
  <https://www.home-connect.com/us/en/meta-pages/reporting-vulnerabilities>
- PGP fingerprint, from the PSIRT pages:
  `0C19 7DE3 6D10 6637 CFDA DFC9 69A8 FAA2 DBBE 481F`

BSH promises an acknowledgement within one month. It also promises at
least one status update within one year. Its reporting pages state both
terms.

## What the exporter holds

- The OAuth refresh token is a credential. It travels only through the
  environment and the TLS connection to Home Connect. The `hce authorize`
  command prints it once, so the operator can store it. The exporter never
  logs it after that.
- The exporter requests the read-only scopes `IdentifyAppliance` and
  `Monitor`. It cannot start or stop programs, and it cannot change
  settings.
- The scrape endpoint publishes appliance names and states. Keep it
  behind your own network boundary when those names are private to you.
