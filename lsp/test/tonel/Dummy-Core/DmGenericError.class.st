Class {
	#name : 'DmSubError',
	#superclass : 'DmError',
	#instVars : [
		'errorPrefix'
	],
	#category : 'Dummy-Core',
	#package : 'Dummy-Core'
}

{ #category : 'instance creation' }
DmSubError class >> classMethodA [
	^ self new
]

{ #category : 'testing' }
DmSubError >> testMethodA [
	^ 'NOAUTH'
]
