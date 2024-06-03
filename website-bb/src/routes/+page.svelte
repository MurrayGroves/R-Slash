<script lang="ts">
    import Paper from '@smui/paper';
    import Button, { Label } from '@smui/button';
    import Card, { Media, Content, MediaContent } from '@smui/card'
    import IconButton from '@smui/icon-button';
    import { Icon } from '@smui/common';

    import { setContext, onMount, onDestroy } from 'svelte';
    import { slide, crossfade } from 'svelte/transition';
    import { circInOut, linear } from 'svelte/easing'
	import { writable } from 'svelte/store';

	// Create a store and update it when necessary...
	const currentSubreddit = writable<string>();
	$: currentSubreddit.set("hentai");

	// ...and add it to the context for child components to access
	setContext('currentSubreddit', currentSubreddit);

    // Create a store and update it when necessary...
	const currentSubredditText = writable<string>();
	$: currentSubredditText.set("hentai");

    const animateSubredditSwitch = (to: string) => {
        let deleting = true;
        const interval = setInterval(() => {
            if (deleting && $currentSubredditText.length != 0 ) {
                currentSubredditText.set($currentSubredditText.slice(0, $currentSubredditText.length - 1))
            } else if ($currentSubredditText.length == 0) {
                deleting = false
                currentSubredditText.set(to.slice(0, $currentSubredditText.length + 2))
            } else {
                currentSubredditText.set(to.slice(0, $currentSubredditText.length + 2))
            }

            if ($currentSubredditText === to) {
                clearInterval(interval)
            }

        }, 100)
    }

    onMount(() => {
        const subreddits = ["rule34", "nsfw", "cumsluts", "nsfw_gif", "gonewild", "milf"]
        currentSubreddit.set("hentai")
        currentSubredditText.set("hentai")
        const subredditSlider = setInterval(() => {
            const current = subreddits.indexOf($currentSubreddit);
            let newIdx;
            if (current + 2 > subreddits.length) {
                newIdx = 0;
            } else {
                newIdx = current + 1;
            }
            currentSubreddit.set(subreddits[newIdx]);
            animateSubredditSwitch(subreddits[newIdx]);
        }, 2000);
    })
</script>

<title>Booty Bot</title>

<div style="display: flex; flex-direction: column; width: 100vw; background-color: var(--background-color-light); padding: 1px">
    <Paper elevation={10} square style="flex: 7; background-color: var(--background-color-dark); padding-left: 0.4%; display: flex">
        <div style="margin-left: 0%; margin-top: 0%; display: flex; padding: 0.8%; align-items: center; gap: 0%; height: 100%; padding: 0.1em; flex: 1">
            <img src="/logo.png" alt="logo" style="height: 3em; margin:0; margin-right: -0.8em" class="rotate270"/>
            <h1 style="margin: 0; font-size: 250%; font-family: 'Lilita One'; color: var(--text-color)">ooty</h1>
            <img src="/logo.png" alt="logo" style="height: 3em; margin:0; margin-right: -0.8em" class="rotate270"/>
            <h1 style="margin: 0; font-size: 250%; font-family: 'Lilita One'; color: var(--text-color)">ot</h1>
        </div>
        <div style="flex: 7; width: 100%">

        </div>
        <div style="flex: 2; display: flex; align-items: center; height: 100%; gap: 5%; justify-content: right; margin-right: 1%">
            <Button variant="raised" style="background-color: var(--accent-color)">
                <Label>Add To Server</Label>
            </Button>
            <Button variant="raised" style="background-color: var(--accent-color)">
                <Label>Get Help</Label>
            </Button>
        </div>
    </Paper>
    
    <div style="flex: 93; padding: 1px">
        <div style="display: flex; margin-top: 12%; margin-left: 18%; padding: 1px">
            <div style="width: 100%; display: grid; overflow:hidden; padding: 1px; flex: 1">
                <div>
                    <p style="font-size: 5em; margin: 0; float:left; color:var(--accent-color)">
                        r/
                    </p>
                    {#key $currentSubredditText}
                    <p style="height: '10px'; font-size: 5em; margin: 0; color:var(--accent-color);">
                        {$currentSubredditText}
                    </p>
                    {/key}
                </div>

                <p style="font-size: 2em; margin: 0">
                    In <span style="color: #5865F2">Discord.</span><br/>
                    What more could you ask for?
                </p>
            </div>
            <div style="flex: 1">
                <Card style="width: 30em; height: 30em; padding: 0.5em; background-color:var(--background-color-dark); display:flex; flex-direction:column">
                    <Media aspectRatio="16x9" style="flex: 20;">
                      <MediaContent>
                        <img src="https://github.com/MurrayGroves/R-Slash/raw/main/docs/get-resized.gif" alt="usage gif" style="width: 29em"/>
                      </MediaContent>
                    </Media>
                    <div style="display: flex">
                        <IconButton class="material-icons"
                            ><Icon class="material-icons">search</Icon></IconButton>
                    </div>
                    <p style="color:var(--text-color); flex: 1; margin: 0; text-align: center; font-size: 1.5em"><b>Here's some gray text down here.</b></p>
                </Card>
            </div>
        </div>
    </div>
</div>
