#!/bin/python3
import sys
import os

toBuild = ['shard', 'downloader', 'role-detector', 'discord-interface', 'rslash-manager']
imageNames = {"shard": "discord-shard", "downloader": "reddit-downloader", "role-detector": "role-detector", "discord-interface": "discord-interface"}

# If there are arguments, only build those
if (len(sys.argv) > 1 and '--dev' not in sys.argv):
    toBuild = sys.argv[1:]

elif len(sys.argv) > 2:
    toBuild = sys.argv[2:]

devBuild = '--dev' in sys.argv

# Build each project
for project in toBuild:
    print('Building ' + project + '...')
    os.system(f"cd {project} && cargo build {'--release' if not devBuild else ''}")
    if project != "rslash-manager":
        print("Building docker image...")
        os.system(f"cd {project} && docker build -t registry.murraygrov.es/{imageNames[project]}{':dev' if devBuild else ''} --build-arg package={'debug' if devBuild else 'release'} .")
        print("Pushing docker image...")
        os.system(f"cd {project} && docker push registry.murraygrov.es/{imageNames[project]}{':dev' if devBuild else ''}")

