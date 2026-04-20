<script lang="ts">
	import { cn } from '$lib/utils';

	let { class: className = '', children } = $props<{
		class?: string;
		children?: import('svelte').Snippet;
	}>();
</script>

<h2 class={cn('text-lg font-semibold tracking-tight', className)}>
	{@render children?.()}
</h2>
