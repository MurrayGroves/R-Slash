#!/bin/python3
# Move from all posts being in subreddit:<subreddit>:posts to being split across subreddit:<subreddit>:posts:<format>

import json

import redis

r = redis.Redis(host="100.96.244.19")

try:
    all_posts = json.load(open("all_posts.json", "r"))
except:
    all_posts = {}
    for key in r.scan_iter("subreddit:*:posts"):
        key = key.decode("utf-8")
        if key.count(":") != 2:
            continue

        print(key)
        all_posts[key] = [myStr.decode("utf-8") for myStr in r.lrange(key, 0, -1)]

    json.dump(all_posts, open("all_posts.json", "w+"))

count = 0
total = len(all_posts)
try:
    new_lists = json.load(open("new_lists.json", "r"))
except:
    new_lists = {}
    for subreddit_key, subreddit_posts in all_posts.items():
        both = []
        text_only = []
        media_only = []
        for post_key in subreddit_posts:
            pipe = r.pipeline()
            pipe.hget(post_key, "embed_url")
            pipe.hget(post_key, "text")
            pipe.hget(post_key, "linked_url")
            media, text, linked_url = pipe.execute()

            has_media = media is not None and len(media) != 0
            has_text = text is not None or linked_url is not None
            if has_media and has_text:
                both.append(post_key)
            elif has_media:
                media_only.append(post_key)
            else:
                text_only.append(post_key)

        new_lists[subreddit_key + ":both"] = both
        new_lists[subreddit_key + ":media"] = media_only
        new_lists[subreddit_key + ":text"] = text_only
        count += 1
        print(f"{count}/{total}")

    json.dump(new_lists, open("new_lists.json", "w+"))

print(
    f"{sum(map(lambda x: len(x), new_lists.values()))} posts over {len(new_lists) / 3} subreddits"
)

count = 0
total = len(new_lists)
for key, posts in new_lists.items():
    r.delete(key)
    if len(posts) != 0:
        r.lpush(key, *posts)
    else:
        print(f"{key} is empty")
    count += 1
    print(f"{count}/{total}")
