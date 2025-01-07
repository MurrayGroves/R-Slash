<script>

	import { Bot } from '$lib/types';

	import BootyBotLogo from '$lib/assets/bootybot.png?enhanced';
	import RSlashLogo from '$lib/assets/rslash.png?enhanced';

	import { fly } from 'svelte/transition';


	let { data } = $props();

	let botProfile = data.bot === Bot.BB ? BootyBotLogo : RSlashLogo;
	let nsfw = data.bot === Bot.BB;

	const subreddits = nsfw ? ['nsfw', 'gonewild'] : ['aww', 'space'];
	let currentSubreddit = $state(0);

	setInterval(() => {
		currentSubreddit = currentSubreddit === subreddits.length - 1 ? 0 : currentSubreddit + 1;
	}, 1500);
</script>

<div class="bg-slate-800 min-h-screen text-gray-200">
	<div class="bg-slate-900 min-h-20 ">
		<div class="flex">
			<enhanced:img src={botProfile} class="size-[4rem] rounded-full aspect-square m-3 mx-3 md:mx-5" alt="Bot Logo" />
			<h1 class="my-auto text-2xl md:text-4xl">
				{data.bot}
			</h1>
			<div class="ml-auto mr-3 md:mr-[15em] my-auto bg-[#5865F2] p-2.5 rounded-md">
				<p>Login with Discord</p>
			</div>
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
