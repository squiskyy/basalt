# Cross-Agent Memory Sharing Implementation Plan

**Goal:** Add first-class cross-agent memory sharing to Basalt so agents can explicitly grant, revoke, and query read/read-write access to their namespaces by other agents.

**Architecture:** A new ShareStore component (concurrent HashMap) tracks sharing policies. Auth middleware in both HTTP and RESP layers is extended to resolve sharing grants. New HTTP endpoints and RESP commands provide the API. Key prefix scoping for fine-grained access. Revocation is immediate.

**Key decisions:**
- ShareStore is separate from AuthStore (identity vs. grants)
- Sharing augments auth, not replaces it
- No shared namespaces or copy-on-write in v1
- In-memory only (not persisted to snapshots in v1)
- No sharing chains (only namespace owners can grant)
- Audit logging via tracing

**Tasks:**
1. Create SharePolicy, SharePermission, ShareStore in src/store/share.rs
2. Wire ShareStore into AppState and server startup
3. Add HTTP share endpoints (POST/DELETE/GET /share, GET /shared-with-me)
4. Extend HTTP auth middleware to check sharing policies
5. Add RESP share commands (SHARE, SHAREDEL, SHARELIST, SHAREWITH)
6. Integration tests for HTTP share endpoints
7. Unit tests for ShareStore
8. Full test suite, fmt, clippy
9. Push and close issue 41
