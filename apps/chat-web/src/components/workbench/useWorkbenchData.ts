import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Api } from "../../api/resources";
import type { AppConfig, HITLConfig } from "../../api/types";

export function useWorkbenchData(search: string, conversationId?: string) {
  const queryClient = useQueryClient();
  const config = useQuery({ queryKey: ["config"], queryFn: Api.config });
  const roles = useQuery({ queryKey: ["roles"], queryFn: Api.roles });
  const projects = useQuery({ queryKey: ["projects"], queryFn: Api.projects });
  const conversations = useQuery({ queryKey: ["conversations", search], queryFn: () => Api.conversations(search) });
  const conversation = useQuery({
    queryKey: ["conversation", conversationId],
    queryFn: () => Api.conversation(conversationId!),
    enabled: Boolean(conversationId)
  });
  const tasks = useQuery({
    queryKey: ["tasks"],
    queryFn: Api.tasks,
    refetchOnWindowFocus: true,
    refetchInterval: false
  });
  const hitl = useQuery({
    queryKey: ["hitl", conversationId],
    queryFn: () => Api.hitlConfig(conversationId!),
    enabled: Boolean(conversationId)
  });
  const pending = useQuery({
    queryKey: ["hitl-pending", conversationId],
    queryFn: () => Api.hitlPending(conversationId),
    enabled: Boolean(conversationId),
    refetchOnWindowFocus: true,
    refetchInterval: false
  });
  const runtimeTodos = useQuery({
    queryKey: ["runtime-todos", conversationId],
    queryFn: () => Api.runtimeTodos(conversationId!),
    enabled: Boolean(conversationId),
    refetchOnWindowFocus: true,
    refetchInterval: false
  });

  const createConversation = useMutation({
    mutationFn: () => Api.createConversation("New Chat"),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["conversations"] })
  });
  const renameConversation = useMutation({
    mutationFn: ({ id, title }: { id: string; title: string }) => Api.renameConversation(id, title),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["conversations"] })
  });
  const deleteConversation = useMutation({
    mutationFn: Api.deleteConversation,
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["conversations"] })
  });
  const setProject = useMutation({
    mutationFn: ({ id, projectId }: { id: string; projectId: string }) => Api.setConversationProject(id, projectId),
    onSuccess: (_, vars) => {
      queryClient.invalidateQueries({ queryKey: ["conversation", vars.id] });
      queryClient.invalidateQueries({ queryKey: ["conversations"] });
    }
  });
  const updateConfig = useMutation({
    mutationFn: (next: AppConfig) => Api.updateConfig(next),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["config"] })
  });
  const listModels = useMutation({ mutationFn: Api.listModels });
  const saveHitl = useMutation({
    mutationFn: ({ id, hitl }: { id: string; hitl: HITLConfig }) => Api.saveHitlConfig(id, hitl),
    onSuccess: (_, vars) => queryClient.invalidateQueries({ queryKey: ["hitl", vars.id] })
  });
  const decideHitl = useMutation({
    mutationFn: ({ id, decision, comment }: { id: string; decision: "approve" | "reject"; comment?: string }) =>
      Api.decideHitl(id, decision, comment),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["hitl-pending"] })
  });

  return {
    config,
    roles,
    projects,
    conversations,
    conversation,
    tasks,
    hitl,
    pending,
    runtimeTodos,
    createConversation,
    renameConversation,
    deleteConversation,
    setProject,
    updateConfig,
    listModels,
    saveHitl,
    decideHitl
  };
}
