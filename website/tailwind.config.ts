import containerQueries from '@tailwindcss/container-queries';
import forms from '@tailwindcss/forms';
import typography from '@tailwindcss/typography';
import type { Config } from 'tailwindcss';

export default {
	content: [
		'./src/**/*.{html,js,svelte,ts}',
		'./node_modules/@mediakular/gridcraft/dist/themes/**/*.svelte'
	],

	theme: {
		extend: {
			fontFamily: {
				sans: ['Open Sans', 'sans-serif']
			},
			colors: {
				'slate-850': 'oklch(25.9% 0.041 260.031)'
			}
		}
	},

	plugins: [typography, forms, containerQueries]
} satisfies Config;
