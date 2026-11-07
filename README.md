# sqlite 0.39 bound patches

These are the crates.io / presage sources thinwire already builds, with one
change each so `libsqlite3-sys` 0.39 can sit in the same graph as rusqlite 0.41.

- `diesel` 2.3.14: `libsqlite3-sys` upper bound `<0.39.0` to `<0.40.0`
- `sqlx-sqlite` 0.9.0: `libsqlite3-sys` upper bound `<0.38.0` to `<0.40.0`
- `presage-store-sqlite` at presage `4b81740610c301cbd54d95f4dbd4cadd692af95e`:
  `libsqlite3-sys` `0.36.0` to `0.39`. The `presage` path dependency is the
  same git revision, so this crate builds outside the presage workspace.

Nothing else in these trees is intentionally different from those sources.
