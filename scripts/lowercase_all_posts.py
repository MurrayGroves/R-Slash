#!/bin/python3

import redis
import json

r = redis.Redis(host='100.96.244.19')

i = 0
for key in r.scan_iter("subreddit:*:post:*"):
    if key.lower() != key:
        if i % 100 == 0:
            print(f"Lowercasing {key.decode('utf-8')} which is key {i}")
        r.rename(key, key.lower())
        i += 1

print(f"Lowercased {i} posts")