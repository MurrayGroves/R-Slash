<script lang="ts">

	import { fly } from 'svelte/transition';
	import { Button } from 'flowbite-svelte';
	import { onMount } from 'svelte';
	import { Backend, checkLogin, getConfig } from '$lib/shared';
	import { botShorthand, prettyPrintBot } from '$lib/types';

	let { data } = $props();

	const config = getConfig(data.bot);

	let currentSubreddit = $state(0);

	setInterval(() => {
		currentSubreddit = (currentSubreddit + 1) % config.subreddits.length;
	}, 1500);

	let loggedIn = $state(false);

	onMount(async () => {
		loggedIn = await checkLogin();
	});
</script>

<div class="bg-slate-800 min-h-screen text-gray-200">
	<div class="bg-slate-900 min-h-20 ">
		<div class="flex w-full">
			<enhanced:img src={config.botProfile} class="size-[4rem] rounded-full aspect-square m-3 mx-3 md:mx-5 flex-1"
										alt="Bot Logo" />
			<h1 class="my-auto text-2xl md:text-4xl flex-1">
				{prettyPrintBot(data.bot)}
			</h1>
			{#if !loggedIn}
				<Button class="self-end flex-10 md:mr-[3em] my-auto bg-[#5865F2] p-2.5 rounded-md min-w-32 font-bold" on:click={async () => {
				await Backend.create(data.bot);
			}}>
					Login with Discord
				</Button>
			{:else}
				<Button class="self-end flex-10 md:mr-[3em] my-auto bg-[#5865F2] p-2.5 rounded-md min-w-32 font-bold" on:click={() => {
					window.location.href = `${botShorthand(data.bot)}/settings`
				}}>
					Settings
				</Button>
			{/if}
		</div>
	</div>


	<div class=" ml-[15%] mt-[15%]">
		<p class="text-6xl leading-normal">
			Your <span class="text-[#5865F2]">Discord</span> Bot <br> for <span
			class="text-[#FF4500]">Reddit</span> {config.nsfw ? "Porn" : "Posts"}
		</p>

		<p>
			Featuring r/
			{#key currentSubreddit}
				<span style={"position: absolute"} in:fly={{delay: 0, duration: 600, y: -20}}
							out:fly={{delay: 0, duration: 600, y: 20}}>{config.subreddits[currentSubreddit]}</span>
			{/key}
		</p>
	</div>
</div>
