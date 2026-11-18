## rxh-bench results

5s per scenario, 32 connections

| Scenario | Target | Verdict | Req/s | p50 | p90 | p99 | Errors | Distribution | Notes |
|---|---|---|---:|---:|---:|---:|---:|---|---|
| throughput | rxh | PASS | 138548 | 0.20ms | 0.38ms | 0.64ms | 0 | 173229 / 173229 / 173229 / 173228 | spread off by 0.0pp max, 0 errors |
| throughput | caddy | PASS | 88232 | 0.28ms | 0.71ms | 1.32ms | 0 | 110306 / 110307 / 110306 / 110306 | spread off by 0.0pp max, 0 errors |
| throughput | nginx | PASS | 218945 | 0.11ms | 0.29ms | 0.46ms | 0 | 273744 / 273744 / 273743 / 273743 | spread off by 0.0pp max, 0 errors |
| throughput | traefik | PASS | 114217 | 0.25ms | 0.43ms | 0.97ms | 0 | 142785 / 142784 / 142784 / 142785 | spread off by 0.0pp max, 0 errors |
| weighted | rxh | PASS | 140674 | 0.20ms | 0.37ms | 0.62ms | 0 | 117236 / 351708 / 234472 | weights 1:3:2, off by 0.0pp max |
| weighted | caddy | PASS | 87920 | 0.28ms | 0.71ms | 1.34ms | 0 | 73278 / 219836 / 146556 | weights 1:3:2, off by 0.0pp max |
| weighted | nginx | PASS | 249433 | 0.10ms | 0.27ms | 0.41ms | 0 | 207903 / 623710 / 415807 | weights 1:3:2, off by 0.0pp max |
| weighted | traefik | PASS | 123621 | 0.22ms | 0.41ms | 0.92ms | 0 | 103028 / 309084 / 206056 | weights 1:3:2, off by 0.0pp max |
| least-conn | rxh | PASS | 128661 | 0.15ms | 0.28ms | 0.52ms | 0 | 949 / 324483 / 324253 | slow backend got 0.1% (round robin would give 33%) |
| least-conn | caddy | PASS | 86823 | 0.20ms | 0.48ms | 1.04ms | 0 | 976 / 218157 / 217895 | slow backend got 0.2% (round robin would give 33%) |
| least-conn | nginx | PASS | 278087 | 0.06ms | 0.15ms | 0.28ms | 0 | 936 / 701950 / 701620 | slow backend got 0.1% (round robin would give 33%) |
| least-conn | traefik | PASS | 109913 | 0.17ms | 0.28ms | 0.72ms | 0 | 1082 / 276550 / 276637 | slow backend got 0.2% (round robin would give 33%) |
| ip-hash | rxh | PASS | 142265 | 0.19ms | 0.37ms | 0.61ms | 0 | 178092 / 222125 / 311170 | 32 keys, 0 hit more than one backend, 3 backends used |
| ip-hash | caddy | PASS | 87235 | 0.28ms | 0.72ms | 1.33ms | 0 | 136407 / 163614 / 136220 | 32 keys, 0 hit more than one backend, 3 backends used |
| ip-hash | nginx | PASS | 245292 | 0.10ms | 0.27ms | 0.42ms | 0 | 458054 / 231421 / 537752 | 32 keys, 0 hit more than one backend, 3 backends used |
| ip-hash | traefik | PASS | 120854 | 0.23ms | 0.42ms | 0.94ms | 0 | 283219 / 132181 / 188923 | 32 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | rxh | PASS | 114083 | 0.24ms | 0.45ms | 0.91ms | 0 | 178285 / 187194 / 205026 | 64 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | caddy | PASS | 84189 | 0.29ms | 0.74ms | 1.40ms | 0 | 138131 / 138143 / 144731 | 64 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | nginx | PASS | 233607 | 0.10ms | 0.28ms | 0.44ms | 0 | 346859 / 365073 / 456320 | 64 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | traefik | n/a | - | - | - | - | - |  | no `uri_hash` policy (Traefik hashes only the client IP) |
| header | rxh | PASS | 135184 | 0.20ms | 0.39ms | 0.65ms | 0 | 264068 / 232375 / 179567 | 64 keys, 0 hit more than one backend, 3 backends used |
| header | caddy | PASS | 85445 | 0.28ms | 0.74ms | 1.33ms | 0 | 133526 / 146867 / 146888 | 64 keys, 0 hit more than one backend, 3 backends used |
| header | nginx | PASS | 239717 | 0.10ms | 0.27ms | 0.43ms | 0 | 355906 / 374638 / 468306 | 64 keys, 0 hit more than one backend, 3 backends used |
| header | traefik | n/a | - | - | - | - | - |  | no `header` policy (Traefik hashes only the client IP) |
| cookie | rxh | PASS | 112793 | 0.26ms | 0.45ms | 0.76ms | 0 | 229034 / 229038 / 105979 | 32 keys, 0 hit more than one backend, 3 backends used |
| cookie | caddy | PASS | 79696 | 0.30ms | 0.79ms | 1.43ms | 0 | 137193 / 124691 / 136643 | 32 keys, 0 hit more than one backend, 3 backends used |
| cookie | nginx | n/a | - | - | - | - | - |  | sticky cookies need NGINX Plus |
| cookie | traefik | PASS | 119561 | 0.23ms | 0.42ms | 0.94ms | 0 | 205557 / 205615 / 186738 | 32 keys, 0 hit more than one backend, 3 backends used |
| failover | rxh | PASS | 142170 | 0.20ms | 0.37ms | 0.61ms | 0 | 77995 / 395944 / 236971 | backend 0 killed after 1.7s: 0 client errors (0.00%) |
| failover | caddy | PASS | 88806 | 0.28ms | 0.70ms | 1.31ms | 0 | 49045 / 197512 / 197511 | backend 0 killed after 1.7s: 0 client errors (0.00%) |
| failover | nginx | PASS | 190324 | 0.12ms | 0.32ms | 0.66ms | 0 | 74634 / 438564 / 438563 | backend 0 killed after 1.7s: 0 client errors (0.00%) |
| failover | traefik | PASS | 110918 | 0.24ms | 0.47ms | 1.06ms | 8 | 62967 / 245855 / 245854 | backend 0 killed after 1.7s: 8 client errors (0.00%) |
| active-health | rxh | PASS | 136747 | 0.20ms | 0.38ms | 0.67ms | 0 | 125533 / 512695 / 319113 | unhealthy backend got 0 requests more than 3s after /health=503, 0 client errors |
| active-health | caddy | PASS | 87950 | 0.28ms | 0.72ms | 1.31ms | 4 | 78314 / 268692 / 268692 | unhealthy backend got 0 requests more than 3s after /health=503, 4 client errors |
| active-health | nginx | n/a | - | - | - | - | - |  | active health checks need NGINX Plus |
| active-health | traefik | PASS | 123563 | 0.22ms | 0.41ms | 0.90ms | 0 | 123044 / 370984 / 370983 | unhealthy backend got 0 requests more than 3s after /health=503, 0 client errors |
| tcp | rxh | PASS | 270016 | 0.10ms | 0.21ms | 0.32ms | 0 | 13514 / 13514 | 0 echo errors, connections per backend [13514, 13514] |
| tcp | caddy | n/a | - | - | - | - | - |  | no layer 4 proxy (Caddy needs the caddy-l4 plugin, pass --caddy-l4) |
| tcp | nginx | PASS | 361123 | 0.07ms | 0.14ms | 0.31ms | 0 | 18065 / 18065 | 0 echo errors, connections per backend [18065, 18065] |
| tcp | traefik | PASS | 140306 | 0.19ms | 0.32ms | 1.31ms | 0 | 7023 / 7023 | 0 echo errors, connections per backend [7023, 7023] |

### Requests per second (and ratio to rxh)

| Scenario | rxh | caddy | nginx | traefik |
|---|---:|---:|---:|---:|
| throughput | 138548 | 88232 (0.64x) | 218945 (1.58x) | 114217 (0.82x) |
| weighted | 140674 | 87920 (0.62x) | 249433 (1.77x) | 123621 (0.88x) |
| least-conn | 128661 | 86823 (0.67x) | 278087 (2.16x) | 109913 (0.85x) |
| ip-hash | 142265 | 87235 (0.61x) | 245292 (1.72x) | 120854 (0.85x) |
| uri-hash | 114083 | 84189 (0.74x) | 233607 (2.05x) | - |
| header | 135184 | 85445 (0.63x) | 239717 (1.77x) | - |
| cookie | 112793 | 79696 (0.71x) | - | 119561 (1.06x) |
| failover | 142170 | 88806 (0.62x) | 190324 (1.34x) | 110918 (0.78x) |
| active-health | 136747 | 87950 (0.64x) | - | 123563 (0.90x) |
| tcp | 270016 | - | 361123 (1.34x) | 140306 (0.52x) |

