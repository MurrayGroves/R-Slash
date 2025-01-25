<script>

	import { Bot, botShorthand } from '$lib/types';

	import { PUBLIC_HOST } from '$env/static/public';

	import BootyBotLogo from '$lib/assets/bootybot.png?enhanced';
	import RSlashLogo from '$lib/assets/rslash.png?enhanced';

	import { fly } from 'svelte/transition';
	import { Button } from 'flowbite-svelte';
	import { UserManager } from 'oidc-client-ts';
	import { onMount } from 'svelte';

	let { data } = $props();

	let botProfile = data.bot === Bot.BB ? BootyBotLogo : RSlashLogo;
	let nsfw = data.bot === Bot.BB;

	const subreddits = nsfw ? ['nsfw', 'gonewild'] : ['aww', 'space'];
	let currentSubreddit = $state(0);

	setInterval(() => {
		currentSubreddit = currentSubreddit === subreddits.length - 1 ? 0 : currentSubreddit + 1;
	}, 1500);

	let userManager = new UserManager({
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
	});

	let user = null;

	onMount(async () => {
		user = await userManager.getUser();
		if (!user) {
			if (window.location.search.includes('code')) {
				user = await userManager.signinCallback(window.location.toString());
				window.location.href = `${PUBLIC_HOST}/${botShorthand(data.bot)}`;
			}
		}

		console.log('User', user);
	});
</script>

<div class="bg-slate-800 min-h-screen text-gray-200">
	<div class="bg-slate-900 min-h-20 ">
		<div class="flex">
			<enhanced:img src={botProfile} class="size-[4rem] rounded-full aspect-square m-3 mx-3 md:mx-5" alt="Bot Logo" />
			<h1 class="my-auto text-2xl md:text-4xl">
				{data.bot}
			</h1>
			<Button class="ml-auto mr-3 md:mr-[15em] my-auto bg-[#5865F2] p-2.5 rounded-md" on:click={() => {
				userManager.signinRedirect();
			}}>
				Login with Discord
			</Button>
		</div>
	</div>


	<div class=" ml-[15%] mt-[15%]">
		<p class="text-6xl leading-normal">
			Your <span class="text-[#5865F2]">Discord</span> Bot <br> for <span
			class="text-[#FF4500]">Reddit</span> {nsfw ? "Porn" : "Posts"}
		</p>

		<p>
			Featuring r/
			{#key currentSubreddit}
				<span style={"position: absolute"} in:fly={{delay: 0, duration: 600, y: -20}}
							out:fly={{delay: 0, duration: 600, y: 20}}>{subreddits[currentSubreddit]}</span>
			{/key}
		</p>


	</div>

</div>
