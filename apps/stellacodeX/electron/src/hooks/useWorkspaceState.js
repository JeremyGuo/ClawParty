import { useCallback, useEffect, useRef, useState } from 'react';
import { loadWorkspaceFile } from '../lib/api';
import { localCacheKey, readLocalCache, removeLocalCache, writeLocalCache } from '../lib/localCache';
import { normalizeWorkspacePath, workspaceFileKind } from '../lib/workspaceUtils';

export function revokeFilePreviewUrls(files = []) {
  files.forEach((file) => {
    if (typeof file?.pdf_url === 'string' && file.pdf_url.startsWith('blob:')) {
      URL.revokeObjectURL(file.pdf_url);
    }
  });
}

function normalizeOpenFileList(files = []) {
  const orderedPaths = [];
  const byPath = new Map();
  const discarded = [];
  files.forEach((file) => {
    const path = normalizeWorkspacePath(file?.path);
    if (!path) return;
    const normalized = { ...file, path };
    if (byPath.has(path)) {
      discarded.push(byPath.get(path));
    } else {
      orderedPaths.push(path);
    }
    byPath.set(path, normalized);
  });
  return {
    files: orderedPaths.map((path) => byPath.get(path)).filter(Boolean),
    discarded
  };
}

export function useWorkspaceState() {
  const [workspaceListings, setWorkspaceListings] = useState(() => new Map());
  const [workspaceExpanded, setWorkspaceExpanded] = useState(() => new Set(['']));
  const [workspaceLoading, setWorkspaceLoading] = useState(() => new Set());
  const [workspaceError, setWorkspaceError] = useState('');
  const [openFiles, setOpenFilesState] = useState([]);
  const [activeFilePath, setActiveFilePath] = useState('');
  const openFilesRef = useRef([]);
  const workspaceResourceCacheRef = useRef(new Map());

  const setOpenFiles = useCallback((updater) => {
    setOpenFilesState((current) => {
      const next = typeof updater === 'function' ? updater(current) : updater;
      const { files: normalized, discarded } = normalizeOpenFileList(Array.isArray(next) ? next : []);
      revokeFilePreviewUrls(discarded);
      openFilesRef.current = normalized;
      return normalized;
    });
  }, []);

  useEffect(() => {
    openFilesRef.current = openFiles;
  }, [openFiles]);

  useEffect(() => () => {
    revokeFilePreviewUrls(openFilesRef.current);
  }, []);

  const resetWorkspaceState = useCallback(() => {
    setWorkspaceListings(new Map());
    setWorkspaceExpanded(new Set(['']));
    setWorkspaceError('');
    setOpenFiles((current) => {
      revokeFilePreviewUrls(current);
      return [];
    });
    setActiveFilePath('');
  }, [setOpenFiles]);

  const restoreOpenFileTabs = useCallback((savedFiles = []) => {
    setOpenFiles((current) => {
      revokeFilePreviewUrls(current);
      return savedFiles.map((file) => {
        const savedKind = workspaceFileKind(file.path);
        return {
          ...file,
          kind: savedKind,
          loading: savedKind !== 'presentation'
        };
      });
    });
  }, [setOpenFiles]);

  const readWorkspaceResourceCache = useCallback((kind, parts) => {
    const key = localCacheKey(kind, parts);
    if (workspaceResourceCacheRef.current.has(key)) {
      return workspaceResourceCacheRef.current.get(key);
    }
    const cached = readLocalCache(kind, parts);
    if (cached !== null) {
      workspaceResourceCacheRef.current.set(key, cached);
    }
    return cached;
  }, []);

  const writeWorkspaceResourceCache = useCallback((kind, parts, value) => {
    const key = localCacheKey(kind, parts);
    workspaceResourceCacheRef.current.set(key, value);
    writeLocalCache(kind, parts, value);
  }, []);

  const removeWorkspaceResourceCache = useCallback((kind, parts) => {
    const key = localCacheKey(kind, parts);
    workspaceResourceCacheRef.current.delete(key);
    removeLocalCache(kind, parts);
  }, []);

  const loadWorkspaceFileCached = useCallback(async (serverId, conversationId, path, limitBytes, options = {}) => {
    const normalized = normalizeWorkspacePath(path);
    const parts = [serverId, conversationId, normalized, limitBytes || 'full'];
    const useCache = options.cache !== false;
    if (useCache && !options.force) {
      const cached = readWorkspaceResourceCache('workspace-file', parts);
      if (cached) return cached;
    }
    const loaded = await loadWorkspaceFile(serverId, conversationId, normalized, limitBytes);
    if (useCache) {
      writeWorkspaceResourceCache('workspace-file', parts, loaded);
    }
    return loaded;
  }, [readWorkspaceResourceCache, writeWorkspaceResourceCache]);

  return {
    workspaceListings,
    setWorkspaceListings,
    workspaceExpanded,
    setWorkspaceExpanded,
    workspaceLoading,
    setWorkspaceLoading,
    workspaceError,
    setWorkspaceError,
    openFiles,
    setOpenFiles,
    activeFilePath,
    setActiveFilePath,
    resetWorkspaceState,
    restoreOpenFileTabs,
    readWorkspaceResourceCache,
    writeWorkspaceResourceCache,
    removeWorkspaceResourceCache,
    loadWorkspaceFileCached
  };
}
