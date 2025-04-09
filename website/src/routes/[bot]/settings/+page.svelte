<script>
	import { Button } from 'flowbite-svelte';
	import { getConfig } from '$lib/shared';

	let data = $props();
	let config = getConfig(data);
</script>

<div class="bg-slate-850 min-h-screen text-gray-200 flex-col flex">
	<div class="bg-slate-900 min-h-20 flex-initial">
		<div class="flex w-full">
			<enhanced:img src={config.botProfile} class="size-[4rem] rounded-full aspect-square m-3 mx-3 md:mx-5 flex-1"
										alt="Bot Logo" />
			<h1 class="my-auto text-2xl md:text-4xl flex-1">
				{data.bot} Settings
			</h1>
			<Button class="self-end flex-10 md:mr-[3em] my-auto bg-[#5865F2] p-2.5 rounded-md min-w-32 font-bold" on:click={async () => {
				await config.userManager.removeUser()
				window.location.href = `/${config.botShorthand}`
			}}>
				Log out
			</Button>
		</div>
	</div>

	<div class="flex w-full min-h-full flex-grow">
		<div class="flex-1 flex-shrink bg-slate-800 p-5">
			<span class="text-2xl font-bold">Servers</span>
			<hr class="mt-1">
		</div>
		<div class="flex-[4] p-5">
			<span class="text-2xl font-bold">Settings for PLACEHOLDER</span>
			<hr class="mt-1">
		</div>
	</div>
</div>