# 6. Garage instead of MinIO

Status: accepted (2026-10-11)

## Context

The local cluster needs an S3-compatible store to stand in for AWS S3, so that
`make up` works on one laptop without an account.

## Decision

Use Garage (`dxflrs/garage:v2.4.1`). MinIO's community edition is archived and
stopped publishing images in October 2025. Garage is S3-compatible, small, written
in Rust, and works with Litestream. The local setup is one node with one fixed,
committed dev key (see [init.sh](../../deploy/garage/init.sh)); it is labelled
"local only".

## Consequences

- Litestream needs path-style addressing for Garage. Setting an endpoint turns it
  on, so the same Litestream config works for AWS S3 (empty endpoint) and Garage.
- Production uses a real S3 service, not Garage. Only the endpoint and the keys in
  the node's env file change.
- Garage needs a one-time layout and bucket setup, which `init.sh` does and can
  repeat safely.
