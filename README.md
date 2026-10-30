# trafficd

trafficd records the per-user traffic of an [Xray](https://github.com/XTLS/Xray-core) server and a [Hysteria 2](https://v2.hysteria.network/) server into SQLite, and serves the statistics over HTTP.

Every `--interval` seconds, and before answering each request, it fetches the counters of both servers, resetting them unless `-R` is given, and appends a row to the `Record` table for each user and server that had traffic.

## Running

```sh
(cd sql && sh create.sh)   # creates sql/traffic.db with the schema in create.sql
trafficd <db path> <users.json> [-b 127.0.0.1:10090 | -b <unix socket path>]
```

`users.json` maps each user name to an object with a numeric `id` and a `uuid`. A user matches the Xray stats of the emails whose part before `@` is the name, and the Hysteria user named by the part of the uuid before its first `-`.

trafficd reads the rest of its configuration from these environment variables:

| variable | value |
| --- | --- |
| `TRAFFICD_TOKEN` | the `Authorization` header every request must carry |
| `TRAFFICD_TZ` | an optional UTC offset such as `+08:00` that delimits months and years, the local offset by default |
| `TRAFFICD_XRAY_SERVER` | the `host:port` of the Xray gRPC API, without a scheme |
| `TRAFFICD_HYSTERIA_URL`, `TRAFFICD_HYSTERIA_SECRET` | the Hysteria traffic stats API URL and its secret |

## API

- `POST /traffic` with a JSON body `{"year": 2026, "month": 10}` returns a protobuf `TrafficStats` from `protos/traffic_stats.proto` for that month. Omitting `month` selects the whole year, and an empty object the current month.
- `GET /traffic/<uid>` returns `{"sum": <bytes>, "until": <unix time>}`, the traffic of that user in the current month and the end of the month.

The `tid` of a row is the user id shifted left by 8 bits, with the low byte naming the server: 0 for Xray, 1 for Hysteria.

## Development

Building needs `protoc`, and the sqlx query macros check the queries against the database that `.env` names, so `sql/traffic.db` must exist. `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test` run in GitHub Actions on every push.
