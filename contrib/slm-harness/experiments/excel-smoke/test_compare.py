import unittest
from proxy import merge_leading_system

class SystemCompatibilityTests(unittest.TestCase):
    def test_merge_keeps_both_policies_before_user_and_does_not_mutate_input(self):
        messages=[{'role':'system','content':'protocol'},{'role':'system','content':'skill'},{'role':'user','content':'task'}]
        result=merge_leading_system(messages)
        self.assertEqual(result,[{'role':'system','content':'protocol\n\nskill'},{'role':'user','content':'task'}])
        self.assertEqual(len(messages),3)
    def test_tool_and_later_messages_are_not_promoted(self):
        messages=[{'role':'system','content':'rules'},{'role':'user','content':'task'},{'role':'tool','content':'data'},{'role':'system','content':'late'}]
        self.assertEqual(merge_leading_system(messages),messages)
    def test_no_system_is_unchanged(self):
        self.assertEqual(merge_leading_system([{'role':'user','content':'task'}]),[{'role':'user','content':'task'}])
if __name__=='__main__':unittest.main()
