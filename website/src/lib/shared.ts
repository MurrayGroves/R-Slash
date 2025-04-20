import { Bot, botClientId, botShorthand, type ChannelSubscription } from '$lib/types';

import { PUBLIC_BACKEND_URL, PUBLIC_HOST } from '$env/static/public';

import BootyBotLogo from '$lib/assets/bootybot.png?enhanced';
import RSlashLogo from '$lib/assets/rslash.png?enhanced';
import { UserManager } from 'oidc-client-ts';
import type { Picture } from 'imagetools-core';

import { page } from '$app/state';

interface ConfigState {
	botProfile: Picture;
	nsfw: boolean;
	bot: Bot;
	subreddits: string[];
	userManager: UserManager;
	botShorthand: string;
}

export interface Guild {
	name: string;
	id: string;
	permissions: number;
}

export interface Channel {
	name: string;
	id: string;
	position: number;
	type: number;
	parent_id: string;
}

export enum TypeOfSelection {
	None,
	Guild,
	Channel
}

/// Login to backend if not logged in
async function login(bot: Bot) {
	if (window.location.search.includes('code')) {
		await fetch(`${PUBLIC_BACKEND_URL}/${bot}/login`, {
			method: 'POST',
			body: JSON.stringify({
				code: page.url.searchParams.get('code'),
				redirect_uri: page.url.href.split('?')[0]
			}),
			headers: {
				'Content-Type': 'application/json'
			},
			credentials: 'include'
		});
		// Strip query params from URL
		const url = new URL(window.location.href);
		url.searchParams.delete('code');
		url.searchParams.delete('state');
		window.location.href = url.href;
	} else {
		const logged_in = await checkLogin();
		if (!logged_in) {
			const state = crypto.randomUUID();
			localStorage.setItem('oauth-state', state);
			const redirect_uri = location.origin + '/' + bot + '/settings';
			window.location.href = `https://discord.com/oauth2/authorize?client_id=${botClientId(bot)}&redirect_uri=${redirect_uri}&response_type=code&scope=identify+email+guilds&prompt=none&state=${state}`;
		}
	}
}

/// Check if logged in to backend
export async function checkLogin() {
	const auth_check = await fetch(`${PUBLIC_BACKEND_URL}/login/check`, {
		credentials: 'include'
	});
	return auth_check.status != 403;
}

export function getConfig(bot: Bot): ConfigState {
	const botProfile = bot === Bot.BB ? BootyBotLogo : RSlashLogo;
	const nsfw = bot === Bot.BB;

	console.log('BOT yh', bot);

	const subreddits = nsfw ? ['nsfw', 'gonewild'] : ['aww', 'space'];

	return {
		botProfile: botProfile,
		nsfw: nsfw,
		bot: bot,
		subreddits: subreddits,
		userManager: new UserManager({
			authority: 'https://discord.com/oauth2/authorize',
			client_id: bot === Bot.BB ? '278550142356029441' : '282921751141285888',
			redirect_uri: `${PUBLIC_HOST}/${botShorthand(bot)}`,
			response_type: 'code',
			scope: 'identify email user',
			post_logout_redirect_uri: `${PUBLIC_HOST}/${botShorthand(bot)}`,
			metadata: {
				issuer: 'https://discord.com',
				authorization_endpoint: 'https://discord.com/oauth2/authorize',
				token_endpoint: 'https://discord.com/api/oauth2/token',
				userinfo_endpoint: 'https://discord.com/api/users/@me',
				end_session_endpoint: 'https://discord.com/api/oauth2/token/revoke'
			}
		}),
		botShorthand: botShorthand(bot)
	};
}

export class Backend {
	private bot: Bot;

	private constructor(bot: Bot) {
		this.bot = bot;
	}

	static async create(bot: Bot): Promise<Backend> {
		await login(bot);
		return new Backend(bot);
	}

	async getGuildsChannels(guilds: Guild[]): Promise<{ [Key: string]: Channel[] }> {
		const queryString = guilds.map((guild) => guild.id).join(',');
		const req = await fetch(
			`${PUBLIC_BACKEND_URL}/${this.bot}/guild/batch/channels?guilds=${queryString}`,
			{
				credentials: 'include'
			}
		);

		if (req.status === 200) {
			return await req.json();
		} else {
			throw new Error('Error fetching guild channels.');
		}
	}

	async getGuilds(): Promise<Guild[]> {
		const endpoint = `${PUBLIC_BACKEND_URL}/guilds`;
		const guilds = await fetch(endpoint, {
			credentials: 'include'
		});
		return await guilds.json();
	}

	async getChannelSubscriptions(channel_id: number | string): Promise<ChannelSubscription[]> {
		const req = await fetch(
			`${PUBLIC_BACKEND_URL}/${this.bot}/channel/${channel_id}/subscriptions`,
			{
				credentials: 'include'
			}
		);

		if (req.status === 200) {
			return await req.json();
		} else {
			throw new Error('Error fetching channel subscriptions.');
		}
	}
}
