<script lang="ts">
	import type { HTMLInputAttributes } from 'svelte/elements';

	import { cn } from '$lib/utils';

	type Props = HTMLInputAttributes & {
		value?: string | number | string[] | undefined;
	};

	let {
		class: className = '',
		value = $bindable(),
		...restProps
	}: Props = $props();
</script>

<input
	bind:value
	class={cn(
		'flex h-11 w-full rounded-lg border border-input bg-background/90 px-3 py-2 text-sm shadow-sm outline-none transition placeholder:text-muted-foreground focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:cursor-not-allowed disabled:opacity-50',
		className
	)}
	{...restProps}
/>
