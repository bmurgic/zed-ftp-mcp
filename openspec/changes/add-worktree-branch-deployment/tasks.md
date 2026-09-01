## 1. Deterministic branch plan and dry run

**Slice state:** [ ] QUEUED

- [ ] 1.1 Add shared branch-deployment contracts, stable manifest statuses, and lexical Git-to-FTP path validation.
- [ ] 1.2 Add temporary Git fixtures and deterministic planning from the full commit range to exact head blob metadata.
- [ ] 1.3 Add deleted-path reporting, dirty-state reporting, and dry-run proof that no blob, credential, or network access occurs.
- [ ] 1.4 Add the `deploy-branch` CLI command and `ftp_deploy_branch` MCP tool with shared dry-run behavior and schemas.
- [ ] 1.5 Add focused documentation and pass the Slice 1 unit, schema, CLI, and MCP suites.
- [ ] 1.V **Slice verification gate**

## 2. Binary upload and in-operation verification

**Slice state:** [ ] QUEUED

- [ ] 2.1 Add the narrow remote protocol, one-process blob reader, in-memory remote, and deterministic executor state machine.
- [ ] 2.2 Add operation-failure continuation, connection-loss cutoff, verification mismatch handling, and complete unsuccessful manifests.
- [ ] 2.3 Add typed `FtpClient` branch operations, explicit binary mode, streamed complete-byte comparison, and one-session execution.
- [ ] 2.4 Enable actual deployment through the existing CLI and MCP surfaces without changing dry-run behavior.
- [ ] 2.5 Add the pinned disposable FTP integration test and pass the Slice 2 executor, adapter, CLI, MCP, and live-server suites.
- [ ] 2.V **Slice verification gate**

## 3. Explicit pinned branch-file deletion

**Slice state:** [ ] QUEUED

- [ ] 3.1 Add deletion contracts and recompute the authorized deleted set from exact base and head commit IDs.
- [ ] 3.2 Add atomic preflight for exact requested paths, required reason, lexical safety, ASCII-only deletion, and ASCII case-collision protection.
- [ ] 3.3 Add deterministic deletion execution with one binary FTP session, individual failure continuation, and connection-loss cutoff.
- [ ] 3.4 Add the `delete-branch-files` CLI command and `ftp_delete_branch_files` MCP tool with complete structured manifests.
- [ ] 3.5 Update the full README workflow and pass compatibility, formatting, lint, workspace, WebAssembly, and disposable FTP suites.
- [ ] 3.V **Slice verification gate**
