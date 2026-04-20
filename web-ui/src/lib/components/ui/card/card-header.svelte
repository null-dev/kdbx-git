<script lang="ts">
	import { cn } from '$lib/utils';

	let { class: className = '', children } = $props<{
		class?: string;
		children?: import('svelte').Snippet;
	}>();
</script>

<div class={cn('flex flex-col gap-1.5 p-6', className)}>
	{@render children?.()}
</div>
