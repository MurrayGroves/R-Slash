#!/bin/python3

import redis
import json

r = redis.Redis(host='100.96.244.19')

try:
    all_posts = json.load(open('all_posts.json', 'r'))
except:
    all_posts = []
    for key in r.scan_iter("subreddit:*:posts"):
        if key.decode('utf-8').count(":") != 2:
            continue

        print(key.decode('utf-8'))
        all_posts.append([myStr.decode('utf-8') for myStr in r.lrange(key.decode('utf-8'), 0, -1)])

    json.dump(all_posts, open('all_posts.json', 'w'))



all_posts = [
    x
    for xs in all_posts
    for x in xs
]

total_posts = len(all_posts)

print(f"Processing {total_posts} posts")

count = 0
invalids = 0
for post in all_posts:
    if count % 100 == 0:
        print(f"Processed {count}/{total_posts} posts")
    count += 1
    res = r.hget(post, 'embed_url')
    if res is None:
        print(f"Removing non-existent post {invalids}, at {count} normal posts, {post}")
        invalids += 1
        r.lrem(f'subreddit:{post.split(":")[1]}:posts', 1, post)
        continue
    res = res.decode("utf-8")
    if "b-cdn" in res:
        r.hset(post, 'embed_url', res.replace("r-slash.b-cdn.net", "cdn.rsla.sh"))

print("Done")
