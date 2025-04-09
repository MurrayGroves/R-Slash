import { Bot, botShorthand } from '$lib/types';

import { PUBLIC_HOST } from '$env/static/public';

import BootyBotLogo from '$lib/assets/bootybot.png?enhanced';
import RSlashLogo from '$lib/assets/rslash.png?enhanced';
import { UserManager } from 'oidc-client-ts';
import type { Picture } from 'imagetools-core';

interface SetupProps {
	bot: Bot;
}

interface ConfigState {
	botProfile: Picture;
	nsfw: boolean;
	bot: Bot;
	subreddits: string[];
	userManager: UserManager;
	botShorthand: string;
}

/// Should be called when page is mounted
export async function getUser(config: ConfigState) {
	let user = null;

	user = await config.userManager.getUser();
	if (!user) {
		if (window.location.search.includes('code')) {
			user = await config.userManager.signinCallback(window.location.toString());
			window.location.href = `${PUBLIC_HOST}/${botShorthand(config.bot)}`;
		}
	}

	return user;
}

export function getConfig(data: SetupProps): ConfigState {
	const botProfile = data.bot === Bot.BB ? BootyBotLogo : RSlashLogo;
	const nsfw = data.bot === Bot.BB;

	const subreddits = nsfw ? ['nsfw', 'gonewild'] : ['aww', 'space'];

	return {
		botProfile: botProfile,
		nsfw: nsfw,
		bot: data.bot,
		subreddits: subreddits,
		userManager: new UserManager({
			authority: 'https://discord.com/oauth2/authorize',
			client_id: data.bot === Bot.BB ? '278550142356029441' : '282921751141285888',
			redirect_uri: `${PUBLIC_HOST}/${botShorthand(data.bot)}`,
			response_type: 'code',
			scope: 'identify email',
			post_logout_redirect_uri: `${PUBLIC_HOST}/${botShorthand(data.bot)}`,
			metadata: {
				issuer: 'https://discord.com',
				authorization_endpoint: 'https://discord.com/oauth2/authorize',
				token_endpoint: 'https://discord.com/api/oauth2/token',
				userinfo_endpoint: 'https://discord.com/api/users/@me',
				end_session_endpoint: 'https://discord.com/api/oauth2/token/revoke'
			}
		}),
		botShorthand: botShorthand(data.bot)
	};
}
