## rxh-bench results

5s per scenario, 32 connections

| Scenario | Target | Verdict | Req/s | p50 | p90 | p99 | Errors | Distribution | Notes |
|---|---|---|---:|---:|---:|---:|---:|---|---|
| throughput | rxh | PASS | 216776 | 0.13ms | 0.20ms | 0.39ms | 0 | 271065 / 271064 / 271064 / 271064 | spread off by 0.0pp max, 0 errors |
| throughput | caddy | PASS | 105769 | 0.23ms | 0.60ms | 0.82ms | 0 | 132236 / 132236 / 132236 / 132236 | spread off by 0.0pp max, 0 errors |
| throughput | nginx | PASS | 309489 | 0.10ms | 0.13ms | 0.19ms | 0 | 386959 / 386959 / 386959 / 386958 | spread off by 0.0pp max, 0 errors |
| throughput | traefik | PASS | 160116 | 0.17ms | 0.30ms | 0.58ms | 0 | 200158 / 200159 / 200159 / 200159 | spread off by 0.0pp max, 0 errors |
| weighted | rxh | PASS | 215578 | 0.13ms | 0.20ms | 0.40ms | 0 | 179661 / 538982 / 359321 | weights 1:3:2, off by 0.0pp max |
| weighted | caddy | PASS | 107980 | 0.23ms | 0.58ms | 0.80ms | 0 | 89988 / 269965 / 179976 | weights 1:3:2, off by 0.0pp max |
| weighted | nginx | PASS | 292459 | 0.11ms | 0.15ms | 0.22ms | 0 | 243737 / 731209 / 487473 | weights 1:3:2, off by 0.0pp max |
| weighted | traefik | PASS | 159188 | 0.17ms | 0.30ms | 0.59ms | 0 | 132667 / 398004 / 265336 | weights 1:3:2, off by 0.0pp max |
| least-conn | rxh | PASS | 197727 | 0.10ms | 0.14ms | 0.28ms | 0 | 966 / 498815 / 498739 | slow backend got 0.1% (round robin would give 33%) |
| least-conn | caddy | PASS | 103843 | 0.17ms | 0.40ms | 0.58ms | 0 | 997 / 261356 / 261607 | slow backend got 0.2% (round robin would give 33%) |
| least-conn | nginx | PASS | 283299 | 0.07ms | 0.11ms | 0.17ms | 0 | 956 / 713768 / 714256 | slow backend got 0.1% (round robin would give 33%) |
| least-conn | traefik | PASS | 137823 | 0.13ms | 0.20ms | 0.46ms | 0 | 1107 / 347957 / 347018 | slow backend got 0.2% (round robin would give 33%) |
| ip-hash | rxh | PASS | 214818 | 0.13ms | 0.20ms | 0.39ms | 0 | 335783 / 402631 / 335754 | 32 keys, 0 hit more than one backend, 3 backends used |
| ip-hash | caddy | PASS | 106384 | 0.23ms | 0.59ms | 0.82ms | 0 | 182901 / 199565 / 149515 | 32 keys, 0 hit more than one backend, 3 backends used |
| ip-hash | nginx | PASS | 322955 | 0.09ms | 0.13ms | 0.17ms | 0 | 714876 / 509393 / 390656 | 32 keys, 0 hit more than one backend, 3 backends used |
| ip-hash | traefik | PASS | 159550 | 0.17ms | 0.30ms | 0.59ms | 0 | 224219 / 274133 / 299455 | 32 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | rxh | PASS | 215685 | 0.13ms | 0.20ms | 0.40ms | 0 | 320204 / 337053 / 421325 | 64 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | caddy | PASS | 106686 | 0.23ms | 0.60ms | 0.82ms | 0 | 225090 / 133381 / 175060 | 64 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | nginx | PASS | 312504 | 0.10ms | 0.14ms | 0.18ms | 0 | 415105 / 537209 / 610459 | 64 keys, 0 hit more than one backend, 3 backends used |
| uri-hash | traefik | n/a | - | - | - | - | - |  | no `uri_hash` policy (Traefik hashes only the client IP) |
| header | rxh | PASS | 213364 | 0.13ms | 0.20ms | 0.41ms | 0 | 350101 / 300089 / 416789 | 64 keys, 0 hit more than one backend, 3 backends used |
| header | caddy | PASS | 105721 | 0.23ms | 0.61ms | 0.83ms | 0 | 165221 / 181752 / 181741 | 64 keys, 0 hit more than one backend, 3 backends used |
| header | nginx | PASS | 321176 | 0.09ms | 0.13ms | 0.18ms | 0 | 577269 / 451776 / 577266 | 64 keys, 0 hit more than one backend, 3 backends used |
| header | traefik | n/a | - | - | - | - | - |  | no `header` policy (Traefik hashes only the client IP) |
| cookie | rxh | PASS | 215534 | 0.13ms | 0.20ms | 0.39ms | 0 | 437730 / 303302 / 336738 | 32 keys, 0 hit more than one backend, 3 backends used |
| cookie | caddy | PASS | 95659 | 0.25ms | 0.65ms | 0.88ms | 0 | 135239 / 224209 / 118890 | 32 keys, 0 hit more than one backend, 3 backends used |
| cookie | nginx | n/a | - | - | - | - | - |  | sticky cookies need NGINX Plus |
| cookie | traefik | PASS | 158070 | 0.17ms | 0.30ms | 0.60ms | 0 | 271938 / 271534 / 246929 | 32 keys, 0 hit more than one backend, 3 backends used |
| failover | rxh | PASS | 213369 | 0.13ms | 0.20ms | 0.39ms | 0 | 119915 / 591360 / 355640 | backend 0 killed after 1.7s: 0 client errors (0.00%) |
| failover | caddy | PASS | 108000 | 0.23ms | 0.59ms | 0.80ms | 0 | 59005 / 240535 / 240534 | backend 0 killed after 1.7s: 0 client errors (0.00%) |
| failover | nginx | PASS | 297968 | 0.08ms | 0.26ms | 0.33ms | 0 | 164751 / 662602 / 662601 | backend 0 killed after 1.7s: 0 client errors (0.00%) |
| failover | traefik | PASS | 148932 | 0.18ms | 0.34ms | 0.61ms | 6 | 82394 / 331165 / 331164 | backend 0 killed after 1.7s: 6 client errors (0.00%) |
| active-health | rxh | PASS | 217008 | 0.13ms | 0.20ms | 0.39ms | 0 | 199676 / 813095 / 506385 | unhealthy backend got 0 requests more than 3s after /health=503, 0 client errors |
| active-health | caddy | PASS | 107415 | 0.22ms | 0.60ms | 0.81ms | 0 | 98538 / 326701 / 326700 | unhealthy backend got 0 requests more than 3s after /health=503, 0 client errors |
| active-health | nginx | n/a | - | - | - | - | - |  | active health checks need NGINX Plus |
| active-health | traefik | PASS | 161176 | 0.17ms | 0.29ms | 0.58ms | 0 | 159187 / 484558 / 484559 | unhealthy backend got 0 requests more than 3s after /health=503, 0 client errors |
| tcp | rxh | PASS | 276361 | 0.11ms | 0.14ms | 0.31ms | 0 | 13828 / 13828 | 0 echo errors, connections per backend [13828, 13828] |
| tcp | caddy | n/a | - | - | - | - | - |  | no layer 4 proxy (Caddy needs the caddy-l4 plugin, pass --caddy-l4) |
| tcp | nginx | PASS | 334747 | 0.09ms | 0.12ms | 0.18ms | 0 | 16746 / 16746 | 0 echo errors, connections per backend [16746, 16746] |
| tcp | traefik | PASS | 228753 | 0.11ms | 0.25ms | 0.50ms | 0 | 11447 / 11447 | 0 echo errors, connections per backend [11447, 11447] |
| tls | rxh | PASS | 203700 | 0.14ms | 0.21ms | 0.40ms | 0 | 509297 / 509297 | HTTP/1.1 over TLS, 0 errors |
| tls | caddy | PASS | 99865 | 0.24ms | 0.63ms | 0.85ms | 0 | 249679 / 249679 | HTTP/1.1 over TLS, 0 errors |
| tls | nginx | PASS | 312769 | 0.10ms | 0.14ms | 0.19ms | 0 | 781995 / 781994 | HTTP/1.1 over TLS, 0 errors |
| tls | traefik | PASS | 155378 | 0.18ms | 0.30ms | 0.60ms | 0 | 388482 / 388482 | HTTP/1.1 over TLS, 0 errors |
| http2 | rxh | PASS | 159387 | 0.19ms | 0.27ms | 0.40ms | 0 | 398488 / 398487 | 796975 of 796975 responses over HTTP/2, 0 errors |
| http2 | caddy | PASS | 75768 | 0.33ms | 0.76ms | 1.07ms | 0 | 189432 / 189432 | 378864 of 378864 responses over HTTP/2, 0 errors |
| http2 | nginx | PASS | 164666 | 0.20ms | 0.24ms | 0.32ms | 0 | 411691 / 411690 | 823381 of 823381 responses over HTTP/2, 0 errors |
| http2 | traefik | PASS | 109140 | 0.26ms | 0.41ms | 0.71ms | 0 | 272864 / 272864 | 545728 of 545728 responses over HTTP/2, 0 errors |
| large-body | rxh | PASS | 1955 | 15.98ms | 20.85ms | 25.67ms | 0 | 4897 / 4897 | 1 MiB up and back per request, 1955 MiB/s each way, 0 errors (wrong echoes included) |
| large-body | caddy | PASS | 2549 | 12.21ms | 16.62ms | 20.89ms | 0 | 6383 / 6383 | 1 MiB up and back per request, 2549 MiB/s each way, 0 errors (wrong echoes included) |
| large-body | nginx | PASS | 1114 | 43.58ms | 45.59ms | 49.02ms | 0 | 2806 / 2806 | 1 MiB up and back per request, 1114 MiB/s each way, 0 errors (wrong echoes included) |
| large-body | traefik | FAIL | 2631 | 11.87ms | 15.72ms | 19.63ms | 8 | 6593 / 6593 | 1 MiB up and back per request, 2631 MiB/s each way, 8 errors (wrong echoes included) |
| websocket | rxh | PASS | 256788 | 0.11ms | 0.15ms | 0.35ms | 0 | 12847 / 12847 | 0 frame echo errors, upgrades per backend [12847, 12847] |
| websocket | caddy | PASS | 184669 | 0.13ms | 0.29ms | 0.56ms | 0 | 9243 / 9244 | 0 frame echo errors, upgrades per backend [9243, 9244] |
| websocket | nginx | PASS | 314435 | 0.09ms | 0.12ms | 0.23ms | 0 | 15730 / 15730 | 0 frame echo errors, upgrades per backend [15730, 15730] |
| websocket | traefik | PASS | 193875 | 0.12ms | 0.27ms | 0.53ms | 0 | 9703 / 9704 | 0 frame echo errors, upgrades per backend [9703, 9704] |
| udp | rxh | PASS | 239114 | 0.13ms | 0.16ms | 0.22ms | 0 | 17 / 16 | 0.00% datagrams lost, sessions per backend [17, 16] |
| udp | caddy | n/a | - | - | - | - | - |  | no layer 4 proxy (Caddy needs the caddy-l4 plugin, pass --caddy-l4) |
| udp | nginx | PASS | 234184 | 0.12ms | 0.21ms | 0.34ms | 0 | 455 / 454 | 0.00% datagrams lost, sessions per backend [455, 454] |
| udp | traefik | PASS | 193351 | 0.16ms | 0.29ms | 0.46ms | 0 | 16 / 17 | 0.00% datagrams lost, sessions per backend [16, 17] |
| idle | rxh | PASS | 218128 | 0.13ms | 0.19ms | 0.48ms | 0 | 550450 / 550449 | 5000 of 5000 idle connections opened, 0 of them failed after the load, 0 load errors |
| idle | caddy | PASS | 126900 | 0.20ms | 0.40ms | 1.18ms | 0 | 322423 / 322423 | 5000 of 5000 idle connections opened, 0 of them failed after the load, 0 load errors |
| idle | nginx | PASS | 321637 | 0.09ms | 0.13ms | 0.18ms | 0 | 809254 / 809253 | 5000 of 5000 idle connections opened, 0 of them failed after the load, 0 load errors |
| idle | traefik | PASS | 154387 | 0.17ms | 0.32ms | 0.59ms | 0 | 391009 / 391009 | 5000 of 5000 idle connections opened, 0 of them failed after the load, 0 load errors |
| memory | rxh | PASS | 35714 | 1.03ms | 1.65ms | 18.23ms | 0 | 2550 / 2550 | 35.7 KiB per idle connection: RSS 19.9 MiB with 100, 194.3 MiB with 5000 more (5000 opened) |
| memory | caddy | PASS | 38145 | 0.80ms | 3.13ms | 14.02ms | 0 | 2550 / 2550 | 33.9 KiB per idle connection: RSS 68.1 MiB with 100, 233.7 MiB with 5000 more (5000 opened) |
| memory | nginx | PASS | 145047 | 0.29ms | 0.44ms | 0.68ms | 0 | 2550 / 2550 | 0.6 KiB per idle connection: RSS 235.8 MiB with 100, 238.7 MiB with 5000 more (5000 opened) |
| memory | traefik | PASS | 36843 | 0.82ms | 2.56ms | 16.73ms | 0 | 2550 / 2550 | 36.0 KiB per idle connection: RSS 112.7 MiB with 100, 288.7 MiB with 5000 more (5000 opened) |

### Requests per second (and ratio to rxh)

| Scenario | rxh | caddy | nginx | traefik |
|---|---:|---:|---:|---:|
| throughput | 216776 | 105769 (0.49x) | 309489 (1.43x) | 160116 (0.74x) |
| weighted | 215578 | 107980 (0.50x) | 292459 (1.36x) | 159188 (0.74x) |
| least-conn | 197727 | 103843 (0.53x) | 283299 (1.43x) | 137823 (0.70x) |
| ip-hash | 214818 | 106384 (0.50x) | 322955 (1.50x) | 159550 (0.74x) |
| uri-hash | 215685 | 106686 (0.49x) | 312504 (1.45x) | - |
| header | 213364 | 105721 (0.50x) | 321176 (1.51x) | - |
| cookie | 215534 | 95659 (0.44x) | - | 158070 (0.73x) |
| failover | 213369 | 108000 (0.51x) | 297968 (1.40x) | 148932 (0.70x) |
| active-health | 217008 | 107415 (0.49x) | - | 161176 (0.74x) |
| tcp | 276361 | - | 334747 (1.21x) | 228753 (0.83x) |
| tls | 203700 | 99865 (0.49x) | 312769 (1.54x) | 155378 (0.76x) |
| http2 | 159387 | 75768 (0.48x) | 164666 (1.03x) | 109140 (0.68x) |
| large-body | 1955 | 2549 (1.30x) | 1114 (0.57x) | 2631 (1.35x) |
| websocket | 256788 | 184669 (0.72x) | 314435 (1.22x) | 193875 (0.76x) |
| udp | 239114 | - | 234184 (0.98x) | 193351 (0.81x) |
| idle | 218128 | 126900 (0.58x) | 321637 (1.47x) | 154387 (0.71x) |
| memory | 35714 | 38145 (1.07x) | 145047 (4.06x) | 36843 (1.03x) |

