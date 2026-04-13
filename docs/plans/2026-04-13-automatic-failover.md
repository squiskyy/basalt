# Automatic Failover for Replication - Implementation Plan

**Goal:** Implement automatic failover for Basalt replication so that when the primary dies, replicas self-promote without human intervention, suitable for Kubernetes deployments.

**Architecture:** Option C from issue #52 - embedded coordinator with leases. Primary sends periodic LEASE heartbeats to replicas. If the lease expires, the most up-to-date replica self-promotes. Epoch-based fencing prevents split-brain. Uses existing infrastructure (replication offsets, WAL streaming) with minimal new code.

---

## Tasks

### Task 1: Add node_id and epoch to ReplicationState
### Task 2: Add failover config and CLI flags
### Task 3: Primary sends LEASE heartbeats
### Task 4: Replica parses LEASE and tracks lease state
### Task 5: Failover monitor background task
### Task 6: Epoch-based fencing
### Task 7: Replica reconfiguration with peer fallback
### Task 8: /ready returns 503 during failover and replica sync
### Task 9: INFO replication updates
### Task 10: Integration tests
### Task 11: Documentation
### Task 12: Final cleanup
