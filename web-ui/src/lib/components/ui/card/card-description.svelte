<script lang="ts">
	import { cn } from '$lib/utils';

	let { class: className = '', children } = $props<{
		class?: string;
		children?: import('svelte').Snippet;
	}>();
</script>

<p class={cn('text-sm leading-6 text-muted-foreground', className)}>
	{@render children?.()}
</p>
