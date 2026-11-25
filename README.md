# Siemens Home Connect exporter

`hce` is a Prometheus exporter for Siemens and Bosch home appliances that
speak Home Connect. It keeps one Server-Sent Events stream open per
appliance and serves the appliance state on `/metrics`.

## How it works

Home Connect appliances report to the BSH cloud. This exporter talks to the
Home Connect API with OAuth. It opens one event stream per appliance. The
stream is free: only the connection setup counts against the daily request
budget of 1000 calls. The exporter keeps 100 calls in reserve and never
polls while a stream is open.

## Metrics

- `homeconnect_appliance_info{ha_id,name,brand,type}` — every registered appliance.
- `homeconnect_connected{appliance}` — 1 while the appliance talks to the cloud.
- `homeconnect_operation_state{appliance,state}` — 1 for the active state, 0 for the rest.
- `homeconnect_door_open{appliance}` — 1 while the door is open.
- `homeconnect_program_progress_percent{appliance}` — program progress.
- `homeconnect_program_time_remaining_seconds{appliance}` — time left.
- `homeconnect_program_time_elapsed_seconds{appliance}` — time in program.
- `homeconnect_program_active{appliance,program}` — 1 for the running program.
- `homeconnect_program_events_total{appliance,event}` — counters, e.g. `ProgramFinished`.
- `homeconnect_up` — 1 after the appliance list call succeeded.
- `homeconnect_api_requests_total{call}` — API calls by type.
- `homeconnect_api_errors_total{call}` — failed API calls by type.

## Configure

All configuration is environment variables.

- `HCE_CLIENT_ID` — required. OAuth client id.
- `HCE_CLIENT_SECRET` — required. OAuth client secret.
- `HCE_REFRESH_TOKEN` — required. Refresh token from the authorize step.
- `HCE_LISTEN_ADDR` — scrape listener. Default `0.0.0.0:9780`.
- `HCE_API_BASE` — API root. Default is the production Home Connect API.
- `HCE_DAILY_REQUEST_BUDGET` — counted calls per day. Default `900`.

## Onboard

The authorize flow runs once. Run it on the machine where your browser
runs, because the redirect returns to that machine's loopback.

1. Create a developer account at `developer.home-connect.com`.
2. Register an application with these values.
   - **Application ID**: any name, for example `hce`. The portal uses it
     as the client id.
   - **OAuth Flow**: `Authorization Code Grant Flow`. The portal cannot
     change this value later.
   - **Home Connect User Account for Testing**: leave it empty. The portal
     then uses the account from your profile, with your real appliances.
   - **Redirect URI**: `http://127.0.0.1:7878/auth`. The exporter builds
     it from its listener address, and the portal must match it exactly.
   - **Enable One Time Token Mode**: leave it off. This mode revokes every
     token when a refresh token is reused, and the exporter reuses its
     refresh token.
   - **Sync to China**: leave it off.
   - **EU Data Act raw data**: leave it off. The exporter does not read
     those events.
3. Save the application. The portal shows its client secret. Set
   `HCE_CLIENT_ID` and `HCE_CLIENT_SECRET` from the application.
4. Run the command:

```console
docker run --rm -it --network host \
  -e HCE_CLIENT_ID=... -e HCE_CLIENT_SECRET=*** \
  ghcr.io/yornik/siemens-home-connect-exporter:latest authorize
```

With a local binary, run `hce authorize` instead. `--network host` puts
the listener on your machine's loopback, so the redirect arrives.

5. The command prints the redirect URI, then one URL. It waits.
6. Open the URL. Log in with your Home Connect account. Approve the access.
7. The command prints the refresh token. Set it as `HCE_REFRESH_TOKEN`.

The exporter requests the scopes `IdentifyAppliance` and `Monitor`. It
cannot start or stop programs.

## Run

```console
docker run -e HCE_CLIENT_ID=... -e HCE_CLIENT_SECRET=... -e HCE_REFRESH_TOKEN=... \
  -p 9780:9780 ghcr.io/yornik/siemens-home-connect-exporter:latest
```

Scrape the exporter:

```yaml
scrape_configs:
  - job_name: homeconnect
    static_configs:
      - targets: ["hce.home:9780"]
```

## Test with simulated appliances

BSH provides a simulator API. It serves fake appliances. A test run needs
no real appliance and no real credential for it.

1. Add a simulated appliance in the developer portal. It pairs to your
   account like a real one.
2. Run the authorize flow with the simulator base:

```console
docker run --rm -it --network host \
  -e HCE_CLIENT_ID=... -e HCE_CLIENT_SECRET=*** \
  -e HCE_API_BASE=https://simulator.home-connect.com \
  ghcr.io/yornik/siemens-home-connect-exporter:latest authorize
```

3. Run the exporter with the same base:

```console
docker run --rm \
  -e HCE_CLIENT_ID=... -e HCE_CLIENT_SECRET=*** -e HCE_REFRESH_TOKEN=*** \
  -e HCE_API_BASE=https://simulator.home-connect.com \
  -p 9780:9780 ghcr.io/yornik/siemens-home-connect-exporter:latest
```

4. Scrape `http://localhost:9780/metrics`. Start a program in the
   simulator. Watch the state, progress, and program-event metrics move.

The same client id and secret work for the simulator. The simulator keeps
its own tokens, so authorize once per base: production and simulator each
get their own refresh token. The exporter sends every request to the
configured base, so no other setting changes.

## Verify a release image

See [docs/verifying-releases.md](docs/verifying-releases.md).
