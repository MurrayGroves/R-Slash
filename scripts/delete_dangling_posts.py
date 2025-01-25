#!/bin/python3

# Delete posts from subreddit lists where the actual post is missing

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

try:
	to_delete = json.load(open('to_delete.json', 'r'))
except:
	to_delete = []
	for posts in all_posts:
		subreddit = posts[0].split(":")[1]
		print(f"Checking {subreddit}")
		for post in posts:
			exists = r.exists(post) > 0
			if not exists:
				to_delete.append(post)


	json.dump(to_delete, open('to_delete.json', 'w'))

go = input(f"Delete {len(to_delete)} posts? (y/n) ")
if go == 'y':
	for post in to_delete:
		r.lrem(f"subreddit:{post.split(':')[1].lower()}:posts", 0, post)
	print("Deleted")
