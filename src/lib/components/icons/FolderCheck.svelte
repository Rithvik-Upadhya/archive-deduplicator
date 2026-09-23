<!-- Folder with a tick. Not in Phosphor; inlined from /folder-check.svg so it
     is bundled (see Icon.svelte on why icons must never be fetched). -->
<script lang="ts">
    let { class: className }: { class?: string } = $props();
</script>

<svg
    xmlns="http://www.w3.org/2000/svg"
    width="1em"
    height="1em"
    viewBox="0 0 24 24"
    class={className}
    aria-hidden="true">
    <path
        fill="currentColor"
        d="M20 4h-8.59L10 2.59C9.63 2.22 9.11 2 8.59 2H4c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V6c0-1.1-.9-2-2-2m-9 11.91L7.29 12.2l1.41-1.41l2.29 2.29l4.29-4.29l1.41 1.41l-5.71 5.71Z" />
</svg>
