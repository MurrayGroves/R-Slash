#!/bin/python3

# Delete posts from subreddit lists where the actual post's media file is missing

import redis
import json
import os
from collections import defaultdict

r = redis.Redis(host='100.96.244.19')

def get_media_filename(media_url):
	# Extract filename from the end of the URL
	return media_url.split('/')[-1]

def media_file_exists(media_url):
	if 'cdn.rsla.sh' in media_url:
		filename = get_media_filename(media_url)
		path = f"/media/pog/rslash-files/{filename}"
		return os.path.exists(path)
	return True  # If not cdn.rsla.sh, assume exists

try:
	all_posts = json.load(open('all_posts.json', 'r'))
except:
	all_posts = []
	for key in r.scan_iter("subreddit:*:posts"):
		if key.decode('utf-8').count(":") != 2:
			continue
		all_posts.append([myStr.decode('utf-8') for myStr in r.lrange(key.decode('utf-8'), 0, -1)])
	json.dump(all_posts, open('all_posts.json', 'w'))

to_delete = []

for posts in all_posts:
	if not posts:
		continue
	subreddit = posts[0].split(":")[1]
	print(f"Checking {subreddit}")
	for post_key in posts:
		# post_key is the key of a hash, get the 'embed_url' field
		media_url = r.hget(post_key, 'embed_url')
		if not media_url:
			print(f"Missing embed_url for {post_key}, marking for deletion")
			to_delete.append((subreddit, post_key))
			continue
		media_url = media_url.decode('utf-8')
		if 'cdn.rsla.sh' in media_url:
			filename = get_media_filename(media_url)
			path = f"/media/pog/rslash-files/{filename}"
			if not os.path.exists(path):
				print(f"Missing file for {post_key}: {path}")
				to_delete.append((subreddit, post_key))

# Group deletions by subreddit

delete_by_subreddit = defaultdict(list)
for subreddit, post_key in to_delete:
	delete_by_subreddit[subreddit].append(post_key)

go = input(f"Delete {len(to_delete)} / {sum([len(posts) for posts in all_posts])} posts? (y/n) ")
if go == 'y':
	for subreddit, post_keys in delete_by_subreddit.items():
		print(f"Deleting {len(post_keys)} posts from subreddit: {subreddit}")
		# Delete post hashes
		for post_key in post_keys:
			r.delete(post_key)
		# Fetch current list, filter, and set new list
		list_key = f"subreddit:{subreddit.lower()}:posts"
		current_posts = [p.decode('utf-8') for p in r.lrange(list_key, 0, -1)]
		new_posts = [p for p in current_posts if p not in post_keys]
		# Replace the list in Redis
		pipe = r.pipeline()
		pipe.delete(list_key)
		if new_posts:
			pipe.rpush(list_key, *new_posts)
		pipe.execute()
		print(f"Updated subreddit list: {subreddit} with {len(new_posts)} posts remaining")
	print("Deleted")
