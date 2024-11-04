import requests
import os
import dotenv
import discord

dotenv.load_dotenv("secrets.env")

POSTHOG_API_KEY = os.getenv("POSTHOG_API_KEY")
POSTHOG_URL = "https://eu.posthog.com/"
POSTHOG_PROJECT_ID = os.getenv("POSTHOG_PROJECT_ID")
POSTHOG_POWER_USERS_ID = os.getenv("POSTHOG_POWER_USERS_ID")

result = requests.get(f"{POSTHOG_URL}api/projects/{POSTHOG_PROJECT_ID}/cohorts/{POSTHOG_POWER_USERS_ID}/persons", headers={"Authorization": f"Bearer {POSTHOG_API_KEY}"}).json()
users = result["results"]

discord_user_ids = [user["distinct_ids"][0].replace("user_", "", 1) for user in users]

print(discord_user_ids)

bot = discord.Bot()

@bot.event
async def on_ready():
    print(f"Logged in as {bot.user.name}")

    for user_id in discord_user_ids:
        try:
            user = await bot.fetch_user(user_id)
            await user.send("""Hello! I've noticed you've been using Booty Bot or R Slash a lot and I'd love to get your feedback. In return you'll receive **six** months of premium for **free**, no card details needed!

All I want is for you to let me know what you like about the bot, what you don't, and what you think I should change.

If that sounds good to you, please join the following Discord server for further details:
https://discord.gg/VweZnWjuM8 
""")
            print(f"Sent message to {user.name}")
        except Exception as e:
            print(f"Failed to send message to {user_id}, {e}")

    print("Done!")

bot.run(os.getenv("DISCORD_BOT_TOKEN"))