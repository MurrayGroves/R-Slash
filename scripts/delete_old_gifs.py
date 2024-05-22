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

to_delete = []
for posts in all_posts:
    for post in posts:
        res = r.hget(post, 'embed_url')
        if res is None:
            to_delete.append(post)
            continue
        url = res.decode('utf-8')
        if "r-slash" in url:
            filename = url.split('/')[-1]
            if len(filename) != 11:
                to_delete.append(post)


json.dump(to_delete, open('to_delete.json', 'w'))

go = input(f"Delete {len(to_delete)} posts? (y/n) ")
if go == 'y':
    for post in to_delete:
        r.delete(post)
    print("Deleted")
